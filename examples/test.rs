use descent::prelude::*;
use flate2::bufread::GzDecoder;
use rand::{
    distributions::{Distribution, Uniform},
    prelude::SliceRandom,
    Rng, SeedableRng,
};
use std::{
    convert::TryInto,
    fs::File,
    io::{self, prelude::*, BufReader, BufWriter},
    path::Path,
};

fn load_gz_bytes(path: impl AsRef<Path>) -> io::Result<Vec<u8>> {
    let reader = BufReader::new(File::open(path).unwrap());
    let mut decoder = GzDecoder::new(reader);
    let mut bytes = Vec::new();
    decoder.read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn read_be_u32(bytes: &[u8]) -> (u32, &[u8]) {
    let (prefix, suffix) = bytes.split_at(4);
    (u32::from_be_bytes(prefix.try_into().unwrap()), suffix)
}

fn read_images_info(bytes: &[u8]) -> ((usize, usize, usize), &[u8]) {
    let (magic, bytes) = read_be_u32(bytes);
    assert_eq!(magic, 2051);
    let (images, bytes) = read_be_u32(bytes);
    let (rows, bytes) = read_be_u32(bytes);
    let (cols, bytes) = read_be_u32(bytes);
    ((images as usize, rows as usize, cols as usize), bytes)
}

fn read_labels_info(bytes: &[u8]) -> (usize, &[u8]) {
    let (magic, bytes) = read_be_u32(bytes);
    assert_eq!(magic, 2049);
    let (items, bytes) = read_be_u32(bytes);
    ((items as usize), bytes)
}

fn unpack_images(
    env: &mut Environment,
    variable: &Variable,
    bytes: &[u8],
    image_base: usize,
    image_count: usize,
) -> io::Result<()> {
    let ((image_end, rows, cols), bytes) = read_images_info(bytes);
    assert!(image_base + image_count <= image_end);
    let pixel_count = rows * cols;
    let mut w = env.writer(variable);
    let mut image = Vec::<f32>::with_capacity(pixel_count);
    for image_index in 0..image_count {
        let begin = (image_base + image_index) * pixel_count;
        let end = begin + pixel_count;
        image.clear();
        image.extend(bytes[begin..end].iter().map(|&c| (c as f32) / 255.0));
        w.write_all(bytemuck::cast_slice(&image))?;
    }
    Ok(())
}

fn unpack_labels(
    env: &mut Environment,
    variable: &Variable,
    bytes: &[u8],
    label_base: usize,
    label_count: usize,
) -> io::Result<()> {
    let (label_end, bytes) = read_labels_info(bytes);
    assert!(label_base + label_count <= label_end);
    let begin = label_base;
    let end = begin + label_count;
    let labels: Vec<f32> = bytes[begin..end].iter().map(|&n| n as f32).collect();
    let mut w = env.writer(variable);
    w.write_all(bytemuck::cast_slice(&labels))
}

fn write_xavier_uniform(env: &mut Environment, variable: &Variable, rng: &mut impl Rng) {
    let shape = variable.shape();

    let mut writer = env.writer(&variable);
    let a = (6.0 / (shape[0] as f32)).sqrt();
    let dist = Uniform::new(-a, a);
    for _ in 0..shape.element_count() {
        let x: f32 = dist.sample(rng);
        writer.write_all(bytemuck::bytes_of(&x)).unwrap();
    }
}

fn softmax_cross_entropy_loss<'g>(z: DualArray<'g>, y: Array<'g>) -> Array<'g> {
    let (z, dz) = (z.value(), z.grad());

    // softmax
    let t = (z - z.reduce_max(-1)).exp();
    let p = t / t.reduce_sum(-1);

    // cross entropy loss
    let h = y.one_hot(10);
    let loss = -(h * p.log()).reduce_sum(-1); // TODO: pick element of p using value of y

    // backprop (softmax with cross entropy directly)
    dz.accumulate(p - h);

    loss
}

fn stochastic_gradient_descent_step(
    graph: &Graph,
    variables: &[Variable],
    mini_batch_size: usize,
    learning_rate: f32,
) {
    graph.next_colour();

    let alpha = learning_rate / (mini_batch_size as f32);
    for var in variables.iter() {
        let g = graph.parameter(var).grad();
        graph.update_variable(var, |theta| theta - alpha * g);
    }
}

fn adam_step(
    env: &mut Environment,
    graph: &Graph,
    variables: &[Variable],
    mini_batch_size: usize,
    learning_rate: f32,
    beta1: f32,
    beta2: f32,
    epsilon: f32,
) {
    graph.next_colour();

    let t_var = env.variable([1], "t");
    env.writer(&t_var).zero_fill();

    let t = graph.update_variable(&t_var, |t| t + 1.0);
    let alpha = learning_rate * (1.0 - (graph.literal(beta2).log() * t).exp()).sqrt()
        / (1.0 - (graph.literal(beta1).log() * t).exp());

    for var in variables.iter() {
        let shape = var.shape();
        let m_var = env.variable(shape.clone(), "m");
        let v_var = env.variable(shape.clone(), "v");
        env.writer(&m_var).zero_fill();
        env.writer(&v_var).zero_fill();

        let g = graph.parameter(var).grad();
        let rcp = 1.0 / (mini_batch_size as f32);

        let m = graph.update_variable(&m_var, |m| m * beta1 + g * ((1.0 - beta1) * rcp));
        let v = graph.update_variable(&v_var, |v| v * beta2 + g * g * ((1.0 - beta2) * rcp * rcp));

        graph.update_variable(var, |theta| theta - alpha * m / (v.sqrt() + epsilon));
    }
}

trait Layer {
    #[must_use]
    fn forward_pass<'g>(&self, graph: &'g Graph, input: DualArray<'g>) -> DualArray<'g>;

    fn collect_parameters(&self, _parameters: &mut Vec<Variable>) {}
}

struct Linear {
    w: Variable,
    b: Variable, // TODO: optional bias?
}

impl Linear {
    fn new(env: &mut Environment, input: usize, output: usize, rng: &mut impl Rng) -> Self {
        let w = env.variable([input, output], "w");
        let b = env.variable([output], "b");

        write_xavier_uniform(env, &w, rng);
        env.writer(&b).zero_fill();

        Self { w, b }
    }
}

impl Layer for Linear {
    fn forward_pass<'g>(&self, graph: &'g Graph, input: DualArray<'g>) -> DualArray<'g> {
        let w = graph.parameter(&self.w);
        let b = graph.parameter(&self.b);
        input.matmul(w) + b
    }

    fn collect_parameters(&self, parameters: &mut Vec<Variable>) {
        parameters.push(self.w.clone());
        parameters.push(self.b.clone());
    }
}

struct LeakyRelu {
    amount: f32,
}

impl LeakyRelu {
    fn new(amount: f32) -> Self {
        Self { amount }
    }
}

impl Layer for LeakyRelu {
    fn forward_pass<'g>(&self, _graph: &'g Graph, input: DualArray<'g>) -> DualArray<'g> {
        input.leaky_relu(self.amount)
    }
}

struct LayeredNetwork {
    layers: Vec<Box<dyn Layer>>,
}

impl LayeredNetwork {
    fn new() -> Self {
        Self { layers: Vec::new() }
    }

    fn add_layer(&mut self, layer: impl Layer + 'static) {
        self.layers.push(Box::new(layer));
    }
}

impl Layer for LayeredNetwork {
    fn forward_pass<'g>(&self, graph: &'g Graph, input: DualArray<'g>) -> DualArray<'g> {
        let mut x = input;
        for layer in self.layers.iter() {
            graph.next_colour();
            x = layer.forward_pass(graph, x);
        }
        graph.next_colour();
        x
    }

    fn collect_parameters(&self, parameters: &mut Vec<Variable>) {
        for layer in self.layers.iter() {
            layer.collect_parameters(parameters);
        }
    }
}

fn main() {
    let epoch_count = 40;
    let m = 1000; // mini-batch size

    let mut env = Environment::new();

    let x_var = env.variable([m, 28 * 28], "x");
    let y_var = env.variable([m, 1], "y");

    let mut rng = rand_chacha::ChaCha20Rng::seed_from_u64(0);
    let mut network = LayeredNetwork::new();
    network.add_layer(Linear::new(&mut env, 28 * 28, 300, &mut rng));
    network.add_layer(LeakyRelu::new(0.01));
    network.add_layer(Linear::new(&mut env, 300, 10, &mut rng));

    let loss_sum_var = env.variable([1], "loss");
    let accuracy_sum_var = env.variable([1], "accuracy");

    let train_graph = {
        let graph = env.graph();
        let x = graph.parameter(&x_var);
        let y = graph.read_variable(&y_var);

        // emit all the layers of the network
        let x = network.forward_pass(&graph, x);

        // loss function
        let _loss = softmax_cross_entropy_loss(x, y);

        // update parameters from gradients
        let mut parameters = Vec::new();
        network.collect_parameters(&mut parameters);
        if false {
            stochastic_gradient_descent_step(&graph, &parameters, m, 0.1);
        } else {
            adam_step(&mut env, &graph, &parameters, m, 0.001, 0.9, 0.999, 1.0E-8);
        }

        graph.build_schedule()
    };
    let mut f = BufWriter::new(File::create("train.dot").unwrap());
    train_graph.write_dot(&mut f).unwrap();

    let test_graph = {
        let graph = env.graph();
        let x = graph.parameter(&x_var);
        let y = graph.read_variable(&y_var);

        // emit all the layers of the network
        let x = network.forward_pass(&graph, x);

        // accumulate loss  (into variable)
        let loss = softmax_cross_entropy_loss(x, y);
        graph.update_variable(&loss_sum_var, |loss_sum| loss_sum + loss.reduce_sum(0));

        // accumulate accuracy (into variable)
        let pred = x.value().argmax(-1);
        let accuracy = pred.select_eq(y, graph.literal(1.0), graph.literal(0.0));
        graph.update_variable(&accuracy_sum_var, |accuracy_sum| {
            accuracy_sum + accuracy.reduce_sum(0)
        });

        graph.build_schedule()
    };
    let mut f = BufWriter::new(File::create("test.dot").unwrap());
    test_graph.write_dot(&mut f).unwrap();

    // load training data
    let train_images = load_gz_bytes("data/train-images-idx3-ubyte.gz").unwrap();
    let train_labels = load_gz_bytes("data/train-labels-idx1-ubyte.gz").unwrap();
    let ((train_image_count, train_image_rows, train_image_cols), _) =
        read_images_info(&train_images);
    let (train_label_count, _) = read_labels_info(&train_labels);
    assert_eq!(train_image_count, train_label_count);
    assert_eq!(train_image_count % m, 0);
    assert_eq!(train_image_rows, 28);
    assert_eq!(train_image_cols, 28);

    // load test data
    let test_images = load_gz_bytes("data/t10k-images-idx3-ubyte.gz").unwrap();
    let test_labels = load_gz_bytes("data/t10k-labels-idx1-ubyte.gz").unwrap();
    let ((test_image_count, test_image_rows, test_image_cols), _) = read_images_info(&test_images);
    let (test_label_count, _) = read_labels_info(&test_labels);
    assert_eq!(test_image_count, test_label_count);
    assert_eq!(test_image_count % m, 0);
    assert_eq!(test_image_rows, 28);
    assert_eq!(test_image_cols, 28);

    // run epochs
    for epoch_index in 0..epoch_count {
        // loop over training mini-batches
        let mut batch_indices: Vec<_> = (0..(train_image_count / m)).collect();
        batch_indices.shuffle(&mut rng);
        for batch_index in batch_indices.iter().copied() {
            let first_index = batch_index * m;
            unpack_images(&mut env, &x_var, &train_images, first_index, m).unwrap();
            unpack_labels(&mut env, &y_var, &train_labels, first_index, m).unwrap();
            env.run(&train_graph);
        }

        // loop over test mini-batches to evaluate loss and accuracy
        env.writer(&loss_sum_var).zero_fill();
        env.writer(&accuracy_sum_var).zero_fill();
        for batch_index in 0..(test_image_count / m) {
            let first_index = batch_index * m;
            unpack_images(&mut env, &x_var, &test_images, first_index, m).unwrap();
            unpack_labels(&mut env, &y_var, &test_labels, first_index, m).unwrap();
            env.run(&test_graph);
        }
        let mut total_loss = 0f32;
        let mut total_accuracy = 0f32;
        env.reader(&loss_sum_var)
            .read_exact(bytemuck::bytes_of_mut(&mut total_loss))
            .unwrap();
        env.reader(&accuracy_sum_var)
            .read_exact(bytemuck::bytes_of_mut(&mut total_accuracy))
            .unwrap();
        println!(
            "epoch: {}, loss: {}, accuracy: {}",
            epoch_index,
            total_loss / (test_image_count as f32),
            total_accuracy / (test_image_count as f32)
        );
    }
}
