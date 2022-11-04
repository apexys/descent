use crate::common::*;
use ordered_float::NotNan;
use petgraph::{
    prelude::*,
    visit::{
        IntoEdgeReferences, IntoNodeReferences, NodeIndexable, NodeRef, Topo, VisitMap, Visitable,
    },
};
use slotmap::{SecondaryMap, SlotMap};
use std::{
    collections::{hash_map::DefaultHasher, HashMap},
    convert::TryInto,
    fs::File,
    hash::{Hash, Hasher},
    io, iter, path::PathBuf, process::Stdio,
};
use tinyvec::ArrayVec as TinyVec;

fn get_arg_edge_ids(ops: &OpGraph, node_id: OpNodeId) -> TinyVec<[OpEdgeId; MAX_OP_ARGS]> {
    let mut v = [None; MAX_OP_ARGS];
    let mut n = 0;
    for edge_ref in ops.edges_directed(node_id, Incoming) {
        let edge = edge_ref.weight();
        assert!(v[edge.arg].is_none());
        v[edge.arg] = Some(edge_ref.id());
        n = n.max(edge.arg + 1);
    }
    v[..n].iter().copied().map(|id| id.unwrap()).collect()
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct ArgSource {
    pub(crate) node_id: OpNodeId,
    pub(crate) is_gather: bool,
    pub(crate) view: View,
}

pub(crate) fn get_arg_sources(
    ops: &OpGraph,
    node_id: OpNodeId,
) -> TinyVec<[ArgSource; MAX_OP_ARGS]> {
    get_arg_edge_ids(ops, node_id)
        .iter()
        .copied()
        .map(|edge_id| {
            let (src_node_id, dst_node_id) = ops.edge_endpoints(edge_id).unwrap();
            ArgSource {
                node_id: src_node_id,
                is_gather: ops[dst_node_id].op.is_gather_arg(ops[edge_id].arg),
                view: ops[edge_id].view,
            }
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InitialState {
    Undefined,
    CopyFrom(OpNodeId),
}

#[derive(Debug)]
pub(crate) struct ClusterOutput {
    pub(crate) node_id: OpNodeId,
    pub(crate) initial_state: InitialState,
}

impl ClusterOutput {
    fn new(node_id: OpNodeId) -> Self {
        Self {
            node_id,
            initial_state: InitialState::Undefined,
        }
    }

    fn copy(dst_node_id: OpNodeId, src_node_id: OpNodeId) -> Self {
        Self {
            node_id: dst_node_id,
            initial_state: InitialState::CopyFrom(src_node_id),
        }
    }
}

#[derive(Debug)]
pub(crate) struct Cluster {
    pub(crate) kernel: GenericKernel,
    pub(crate) inputs: Vec<OpNodeId>,
    pub(crate) outputs: Vec<ClusterOutput>,
}

slotmap::new_key_type! {
    pub(crate) struct ClusterId;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KernelDotOutput {
    None,
    Cluster,
    Color,
}

pub struct Graph {
    pub(crate) parameters: SharedParameters,
    pub(crate) ops: OpGraph,
    pub(crate) ops_sorted: Vec<OpNodeId>,
    pub(crate) clusters: SlotMap<ClusterId, Cluster>,
    pub(crate) clusters_sorted: Vec<ClusterId>,
}

fn write_dot(graph: &Graph, path: &str){
    let mut cmd = std::process::Command::new("dot").arg("-Tsvg").stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
    let mut stdin = cmd.stdin.take().unwrap();
    graph.write_dot(KernelDotOutput::Cluster, &mut stdin).unwrap();
    drop(stdin);
    std::fs::write(path, cmd.wait_with_output().unwrap().stdout).unwrap();
}

impl Graph {
    pub(crate) fn new(parameters: SharedParameters, ops: OpGraph) -> Self {
        let mut graph = Self {
            parameters,
            ops,
            ops_sorted: Vec::new(),
            clusters: SlotMap::with_key(),
            clusters_sorted: Vec::new(),
        };

        //write_dot(&graph, "original.svg");

        graph.rebuild_ordering();
        graph.eliminate_dead_code();

        //write_dot(&graph, "after_dead_code.svg");

        graph.rebuild_ordering();
        graph.eliminate_moves();

        //write_dot(&graph, "after_move_elimination.svg");

        graph.rebuild_ordering();
        graph.simplify_arithmetic();

        //write_dot(&graph, "after_simplify_arithmetic.svg");

        graph.rebuild_ordering();
        graph.eliminate_common_subgraphs();

        //write_dot(&graph, "after_eliminate_common_subgraphs.svg");

        graph.rebuild_ordering();
        graph.make_built_ins_and_literals_unique();

        //write_dot(&graph, "after_make_built_ins_and_literals_unique.svg");

        graph.rebuild_ordering();
        graph.build_clusters();

        //write_dot(&graph, "after_build_clusters.svg");
        //write_dot(&graph, "optimized.svg");

        graph
    }

    fn rebuild_ordering(&mut self) {
        self.ops_sorted.clear();
        let mut topo = Topo::new(&self.ops);
        while let Some(node_id) = topo.next(&self.ops) {
            self.ops_sorted.push(node_id);
        }
        assert_eq!(self.ops.node_count(), self.ops_sorted.len());
    }

    fn eliminate_dead_code(&mut self) {
        let mut live = self.ops.visit_map();
        for node_ref in self.ops.node_references() {
            if matches!(node_ref.weight().op, Op::Output { .. }) {
                live.visit(node_ref.id());
            }
        }
        for index in self.ops_sorted.iter().rev().copied() {
            if live.is_visited(&index) {
                for input_index in self.ops.neighbors_directed(index, Incoming) {
                    live.visit(input_index);
                }
            }
        }
        self.ops.retain_nodes(|_, index| live.is_visited(&index));
    }

    fn eliminate_common_subgraphs(&mut self) {
        let mut hashes = vec![0u64; self.ops.node_bound()];
        let mut ids_from_hash = HashMap::new();
        for node_id in self.ops_sorted.iter().copied() {
            let node = &self.ops[node_id];
            let arg_sources = get_arg_sources(&self.ops, node_id);
            let hash = {
                let mut hasher = DefaultHasher::new();
                for arg_source in arg_sources.iter() {
                    arg_source.hash(&mut hasher);
                }
                node.shape.hash(&mut hasher);
                node.op.hash(&mut hasher);
                hasher.finish()
            };
            hashes[node_id.index()] = hash;
            if node.op.can_merge() {
                let ids = ids_from_hash.entry(hash).or_insert_with(Vec::new);
                if let Some(other_id) = ids.iter().copied().find(|&id| {
                    let other_node = &self.ops[id];
                    let other_arg_sources = get_arg_sources(&self.ops, id);
                    node.shape == other_node.shape
                        && node.op == other_node.op
                        && arg_sources == other_arg_sources
                }) {
                    let mut edges = self.ops.neighbors_directed(node_id, Outgoing).detach();
                    while let Some((edge_id, dst_id)) = edges.next(&self.ops) {
                        let edge = self.ops[edge_id].clone();
                        self.ops.add_edge(other_id, dst_id, edge);
                    }
                    self.ops.remove_node(node_id);
                } else {
                    ids.push(node_id);
                }
            }
        }
    }

    fn make_built_ins_and_literals_unique(&mut self) {
        for node_id in self.ops_sorted.iter().copied() {
            let node = &self.ops[node_id];
            if matches!(&node.op, Op::Literal(_) | Op::BuiltIn { .. }) {
                let orig_node = node.clone();
                let mut out_edges = self.ops.neighbors_directed(node_id, Outgoing).detach();
                while let Some((out_edge_id, out_node_id)) = out_edges.next(&self.ops) {
                    let new_node_id = self.ops.add_node(orig_node.clone());
                    let new_edge = self.ops[out_edge_id].clone();
                    self.ops.add_edge(new_node_id, out_node_id, new_edge);
                }
                self.ops.remove_node(node_id);
            }
        }
    }

    fn simplify_arithmetic(&mut self) {
        let mut mov_added = false;
        for node_id in self.ops_sorted.iter().copied() {
            let skip_literal = match &self.ops[node_id].op {
                Op::Binary(BinaryOp::Mul) => Some(Literal::F32(NotNan::new(1.0).unwrap())),
                Op::Binary(BinaryOp::Add) => Some(Literal::F32(NotNan::new(0.0).unwrap())),
                Op::Binary(BinaryOp::UMul) => Some(Literal::U32(1)),
                Op::Binary(BinaryOp::UAdd) => Some(Literal::U32(0)),
                _ => None,
            };
            if let Some(skip_literal) = skip_literal {
                let arg_edge_ids = get_arg_edge_ids(&self.ops, node_id);
                let skip_literal_edge_id = arg_edge_ids.iter().copied().find(|&edge_id| {
                    let src_node_id = self.ops.edge_endpoints(edge_id).unwrap().0;
                    self.ops[src_node_id].op == Op::Literal(skip_literal)
                });
                if let Some(skip_literal_edge_id) = skip_literal_edge_id {
                    for edge_id in arg_edge_ids.iter().copied() {
                        if edge_id == skip_literal_edge_id {
                            self.ops.remove_edge(edge_id);
                        } else {
                            self.ops[node_id].op = Op::Unary(UnaryOp::Mov);
                            self.ops[edge_id].arg = 0;
                        }
                    }
                    mov_added = true;
                }
            }
        }
        if mov_added {
            self.eliminate_moves();
        }
    }

    fn eliminate_moves(&mut self) {
        for node_id in self.ops_sorted.iter().copied() {
            if let Op::Unary(UnaryOp::Mov) = &self.ops[node_id].op {
                assert_eq!(self.ops[node_id].op, Op::Unary(UnaryOp::Mov));
                // attempt to adjust the incoming edge view to match the target shape naturally
                if let Some(in_edge_ref) = self.ops.edges_directed(node_id, Incoming).only(){
                    let in_edge_id = in_edge_ref.id();
                    let in_node_id = in_edge_ref.source();
                    if let Some(view_match) = View::try_from_reshape(
                        self.ops[in_edge_id].view.output_shape,
                        self.ops[node_id].shape,
                    ) {
                        let view = &mut self.ops[in_edge_id].view;
                        *view = view.through(&view_match, false);
                    }
    
                    // then see if we can merge with outgoing edges
                    let can_reshape = self.ops[in_node_id].op.can_reshape();
                    let can_eliminate =
                        self.ops
                            .edges_directed(node_id, Outgoing)
                            .all(|out_edge_ref| {
                                self.ops[out_edge_ref.target()]
                                    .op
                                    .output_parameter_id()
                                    .is_none()
                                    && self.ops[in_edge_id].view.can_view_through(
                                        &self.ops[out_edge_ref.id()].view,
                                        can_reshape,
                                    )
                            });
                    if can_eliminate {
                        let mut out_edges = self.ops.neighbors_directed(node_id, Outgoing).detach();
                        while let Some((out_edge_id, out_node_id)) = out_edges.next(&self.ops) {
                            let in_edge = &self.ops[in_edge_id];
                            let out_edge = &self.ops[out_edge_id];
                            assert_eq!(in_edge.arg, 0);
                            let new_edge = OpEdge {
                                arg: out_edge.arg,
                                view: in_edge.view.through(&out_edge.view, can_reshape),
                            };
                            self.ops.add_edge(in_node_id, out_node_id, new_edge);
                        }
                        self.ops.remove_node(node_id);
                    }
                }else{
                    eprintln!("Cannot eliminate move node {:?} with no incoming edges: {:?}", node_id, &self.ops[node_id]);
                    for e in self.ops.edges(node_id) {
                        eprintln!("from {:?} to {:?}", e.source(), e.target());
                    }
                }
            }
        }
    }

    fn any_predecessor(&self, roots: &[OpNodeId], mut f: impl FnMut(OpNodeId) -> bool) -> bool {
        let mut markers = self.ops.visit_map();
        for &node_id in roots {
            markers.visit(node_id);
        }
        for node_id in self.ops_sorted.iter().copied().rev() {
            if self
                .ops
                .neighbors_directed(node_id, Outgoing)
                .any(|output_node_id| markers.is_visited(&output_node_id))
            {
                markers.visit(node_id);
                if f(node_id) {
                    return true;
                }
            }
        }
        false
    }

    fn any_successor(&self, roots: &[OpNodeId], mut f: impl FnMut(OpNodeId) -> bool) -> bool {
        let mut markers = self.ops.visit_map();
        for &node_id in roots {
            markers.visit(node_id);
        }
        for node_id in self.ops_sorted.iter().copied() {
            if self
                .ops
                .neighbors_directed(node_id, Incoming)
                .any(|input_node_id| markers.is_visited(&input_node_id))
            {
                markers.visit(node_id);
                if f(node_id) {
                    return true;
                }
            }
        }
        false
    }

    #[allow(clippy::blocks_in_if_conditions)]
    fn build_clusters(&mut self) {
        // first gather per-element nodes into kernels
        for first_node_id in self.ops_sorted.iter().copied() {
            let first_node = &self.ops[first_node_id];
            if first_node.cluster_id.is_some() {
                continue;
            }
            if first_node.op.is_per_element() {
                let element_count = first_node.shape.element_count();

                let cluster_id = Some(self.clusters.insert(Cluster {
                    kernel: GenericKernel::PerElement(PerElementKernel {
                        element_count,
                        inputs: Vec::new(),
                        outputs: Vec::new(),
                        ops: Vec::new(),
                    }),
                    inputs: Vec::new(),
                    outputs: Vec::new(),
                }));
                self.ops[first_node_id].cluster_id = cluster_id;

                'outer: loop {
                    'inner: for other_node_id in self.ops_sorted.iter().copied() {
                        let other_node = &self.ops[other_node_id];

                        // check this node has no cluster and matches element count
                        let can_include = other_node.cluster_id.is_none()
                            && other_node.op.is_per_element()
                            && other_node.shape.element_count() == element_count;
                        if !can_include {
                            continue 'inner;
                        }

                        // skip this node if any edges with cluster nodes are not per-element
                        let mut has_kernel_neighbor = false;
                        for edge_ref in self
                            .ops
                            .edges_directed(other_node_id, Incoming)
                            .filter(|edge_ref| {
                                assert_eq!(edge_ref.target(), other_node_id);
                                self.ops[edge_ref.source()].cluster_id == cluster_id
                            })
                            .chain(self.ops.edges_directed(other_node_id, Outgoing).filter(
                                |edge_ref| {
                                    assert_eq!(edge_ref.source(), other_node_id);
                                    self.ops[edge_ref.target()].cluster_id == cluster_id
                                },
                            ))
                        {
                            has_kernel_neighbor = true;
                            if !edge_ref
                                .weight()
                                .is_per_element(&self.ops[edge_ref.target()].op)
                            {
                                continue 'inner;
                            }
                        }

                        // placing this node in the cluster needs to save a load
                        if !has_kernel_neighbor {
                            continue 'inner;
                        }

                        // check uses of this node don't re-enter this cluster
                        if self.any_successor(&[other_node_id], |node_id| {
                            self.ops[node_id].cluster_id.is_none()
                                && self
                                    .ops
                                    .neighbors_directed(node_id, Outgoing)
                                    .any(|node_id| self.ops[node_id].cluster_id == cluster_id)
                        }) {
                            continue 'inner;
                        }

                        // check inputs of this node don't re-enter this cluster
                        if self.any_predecessor(&[other_node_id], |node_id| {
                            self.ops[node_id].cluster_id.is_none()
                                && self
                                    .ops
                                    .neighbors_directed(node_id, Incoming)
                                    .any(|node_id| self.ops[node_id].cluster_id == cluster_id)
                        }) {
                            continue 'inner;
                        }

                        // ok to merge, restart search with new cluster
                        self.ops[other_node_id].cluster_id = cluster_id;
                        continue 'outer;
                    }
                    break 'outer;
                }
            }
        }

        // finally build the per-element clusters and kernels
        for (cluster_id, cluster) in self.clusters.iter_mut() {
            let kernel = match &mut cluster.kernel {
                GenericKernel::PerElement(kernel) => kernel,
                _ => unreachable!(),
            };
            let inputs = &mut cluster.inputs;
            let outputs = &mut cluster.outputs;

            let mut arg_op_index = HashMap::new();
            let mut member_op_index = HashMap::new();

            let ops = &self.ops;
            for node_id in self
                .ops_sorted
                .iter()
                .copied()
                .filter(|&node_id| Some(cluster_id) == ops[node_id].cluster_id)
            {
                // gather the arguments (loading as necessary)
                let arg_sources = get_arg_sources(ops, node_id);
                let args: TinyVec<[usize; MAX_OP_ARGS]> = arg_sources
                    .iter()
                    .map(|source| {
                        if let Some(op_index) = member_op_index.get(&source.node_id) {
                            *op_index
                        } else {
                            *arg_op_index.entry(*source).or_insert_with(|| {
                                if source.is_gather {
                                    let input_index = kernel.inputs.len();
                                    kernel.inputs.push(source.view);
                                    inputs.push(source.node_id);
                                    input_index
                                } else {
                                    let source_node = &ops[source.node_id];
                                    assert_ne!(source_node.cluster_id, Some(cluster_id));
                                    let op_index = kernel.ops.len();
                                    match source_node.op {
                                        Op::Literal(value) => {
                                            kernel.ops.push(PerElementKernelOp::Literal(value));
                                        }
                                        Op::BuiltIn(op) => {
                                            kernel.ops.push(PerElementKernelOp::BuiltIn {
                                                op,
                                                view: source.view,
                                            });
                                        }
                                        _ => {
                                            let input_index = kernel.inputs.len();
                                            kernel.inputs.push(source.view);
                                            inputs.push(source.node_id);
                                            kernel
                                                .ops
                                                .push(PerElementKernelOp::Load { input_index });
                                        }
                                    }
                                    op_index
                                }
                            })
                        }
                    })
                    .collect();

                // emit the op
                if args.len() > 0 {
                    let op = match ops[node_id].op {
                        Op::Unary(op) => PerElementKernelOp::Unary { op, args: args[0] },
                        Op::Binary(op) => PerElementKernelOp::Binary {
                            op,
                            args: args[..2].try_into().unwrap(),
                        },
                        Op::CompareAndSelect(compare_mode) => PerElementKernelOp::CompareAndSelect {
                            compare_mode,
                            args: args[..4].try_into().unwrap(),
                        },
                        Op::Gather { axis } => PerElementKernelOp::Gather {
                            shape: ops[node_id].shape,
                            axis,
                            input_index: args[0],
                            arg: args[1],
                        },
                        _ => panic!("unexpected op type"),
                    };
                    let op_index = kernel.ops.len();
                    kernel.ops.push(op);
                    member_op_index.insert(node_id, op_index);

                    // store the result if necessary
                    if ops
                        .neighbors_directed(node_id, Outgoing)
                        .any(|other_id| ops[other_id].cluster_id != Some(cluster_id))
                    {
                        kernel.outputs.push(op_index);
                        outputs.push(ClusterOutput::new(node_id));
                    }
                }else{
                    eprintln!("Node with no inputs: {:?}", ops[node_id]);
                }
            }
        }

        // add reduction and matrix multiply kernels
        for node_id in self.ops_sorted.iter().copied() {
            let node = &self.ops[node_id];
            if node.cluster_id.is_none() {
                match node.op {
                    Op::Reduce { reduce_op, axis } => {
                        let arg_sources = get_arg_sources(&self.ops, node_id);
                        assert_eq!(arg_sources.len(), 1);
                        let src0 = &arg_sources[0];
                        self.ops[node_id].cluster_id = Some(self.clusters.insert(Cluster {
                            kernel: GenericKernel::Reduce(ReduceKernel {
                                shape: node.shape,
                                input: src0.view,
                                reduce_op,
                                axis,
                            }),
                            inputs: vec![src0.node_id],
                            outputs: vec![ClusterOutput::new(node_id)],
                        }));
                    }
                    Op::MatMul { output_mode } => {
                        let arg_sources = get_arg_sources(&self.ops, node_id);
                        assert_eq!(arg_sources.len(), 2);
                        let a = &arg_sources[0];
                        let b = &arg_sources[1];
                        self.ops[node_id].cluster_id = Some(self.clusters.insert(Cluster {
                            kernel: GenericKernel::MatMul(MatMulKernel {
                                shape: node.shape,
                                output_mode,
                                a: a.view,
                                b: b.view,
                            }),
                            inputs: vec![a.node_id, b.node_id],
                            outputs: vec![ClusterOutput::new(node_id)],
                        }));
                    }
                    Op::Unpad { axis, pad } => {
                        let arg_sources = get_arg_sources(&self.ops, node_id);
                        assert_eq!(arg_sources.len(), 1);
                        let src0 = &arg_sources[0];
                        self.ops[node_id].cluster_id = Some(self.clusters.insert(Cluster {
                            kernel: GenericKernel::Unpad(UnpadKernel {
                                shape: node.shape,
                                input: src0.view,
                                axis,
                                pad,
                            }),
                            inputs: vec![src0.node_id],
                            outputs: vec![ClusterOutput::new(node_id)],
                        }));
                    }
                    Op::WindowsToImage { stride } => {
                        let arg_sources = get_arg_sources(&self.ops, node_id);
                        assert_eq!(arg_sources.len(), 1);
                        let src0 = &arg_sources[0];
                        self.ops[node_id].cluster_id = Some(self.clusters.insert(Cluster {
                            kernel: GenericKernel::WindowsToImage(WindowsToImageKernel {
                                shape: node.shape,
                                input: src0.view,
                                stride,
                            }),
                            inputs: vec![src0.node_id],
                            outputs: vec![ClusterOutput::new(node_id)],
                        }));
                    }
                    Op::ScatterAdd { axis } => {
                        let arg_sources = get_arg_sources(&self.ops, node_id);
                        assert_eq!(arg_sources.len(), 3);
                        let acc = &arg_sources[0];
                        let values = &arg_sources[1];
                        let indices = &arg_sources[2];
                        assert!(
                            acc.view.is_contiguous()
                                || matches!(self.ops[acc.node_id].op, Op::Literal(_))
                        );
                        self.ops[node_id].cluster_id = Some(self.clusters.insert(Cluster {
                            kernel: GenericKernel::ScatterAdd(ScatterAddKernel {
                                shape: node.shape,
                                values: values.view,
                                axis,
                                indices: indices.view,
                            }),
                            inputs: vec![values.node_id, indices.node_id],
                            outputs: vec![ClusterOutput::copy(node_id, acc.node_id)],
                        }));
                    }
                    Op::Input { .. } | Op::Output { .. } | Op::Literal(_) | Op::BuiltIn(_) => {}
                    Op::Unary(..)
                    | Op::Binary(..)
                    | Op::CompareAndSelect(..)
                    | Op::Gather { .. } => unreachable!(),
                }
            }
        }

        // make cluster ordering
        let mut cluster_graph = StableDiGraph::<ClusterId, (), usize>::default();
        let mut cluster_node_ids = SecondaryMap::new();
        for cluster_id in self.clusters.keys() {
            cluster_node_ids.insert(cluster_id, cluster_graph.add_node(cluster_id));
        }
        for (source_id, target_id) in self.ops.edge_references().filter_map(|edge_ref| {
            let source_id = self.ops[edge_ref.source()].cluster_id?;
            let target_id = self.ops[edge_ref.target()].cluster_id?;
            if source_id != target_id {
                Some((source_id, target_id))
            } else {
                None
            }
        }) {
            cluster_graph.add_edge(cluster_node_ids[source_id], cluster_node_ids[target_id], ());
        }
        self.clusters_sorted.clear();
        let mut topo = Topo::new(&cluster_graph);
        while let Some(cluster_node_id) = topo.next(&cluster_graph) {
            self.clusters_sorted.push(cluster_graph[cluster_node_id]);
        }
        assert_eq!(self.clusters_sorted.len(), self.clusters.len());
    }

    pub fn write_dot_file(&self, kernel_output: KernelDotOutput, path: &str) {
        let mut w = io::BufWriter::new(File::create(path).unwrap());
        self.write_dot(kernel_output, &mut w).unwrap();
    }

    fn write_dot(&self, kernel_output: KernelDotOutput, w: &mut impl io::Write) -> io::Result<()> {
        writeln!(w, "digraph G {{")?;
        for (index, cluster_id) in iter::once(None)
            .chain(self.clusters.keys().map(Some))
            .enumerate()
        {
            if kernel_output == KernelDotOutput::Cluster && cluster_id.is_some() {
                writeln!(w, "subgraph cluster{} {{ style=filled;", index)?;
            }
            for node_ref in self
                .ops
                .node_references()
                .filter(|node_ref| node_ref.weight().cluster_id == cluster_id)
            {
                let node = node_ref.weight();
                if let Op::Literal(value) = &node.op {
                    match value {
                        Literal::F32(value) => writeln!(
                            w,
                            "n{} [shape=none,label=\"{:E}\"];",
                            node_ref.id().index(),
                            value.into_inner()
                        )?,
                        Literal::U32(value) => writeln!(
                            w,
                            "n{} [shape=none,label=\"{}\"];",
                            node_ref.id().index(),
                            value
                        )?,
                    }
                } else {
                    let hasher = if kernel_output == KernelDotOutput::Color {
                        cluster_id.map(|cluster_id| {
                            let mut hasher = DefaultHasher::new();
                            cluster_id.hash(&mut hasher);
                            hasher
                        })
                    } else {
                        let mut hasher = DefaultHasher::new();
                        node.colour.hash(&mut hasher);
                        Some(hasher)
                    };
                    let col = if let Some(hasher) = hasher {
                        let hash = hasher.finish();
                        ((((hash >> 48) ^ (hash >> 24) ^ hash) as u32) & 0xffffff) | 0x404040
                    } else {
                        0xffffff
                    };
                    write!(
                        w,
                        "n{} [shape=box,style={},color=\"#{:06X}\",label=\"{}\\n",
                        node_ref.id().index(),
                        if matches!(node.op, Op::Input { .. } | Op::Output { .. }) {
                            "solid"
                        } else {
                            "filled"
                        },
                        col,
                        node.op
                    )?;
                    if let Op::Input { parameter_id } | Op::Output { parameter_id } = node.op {
                        write!(
                            w,
                            "{}",
                            self.parameters
                                .as_ref()
                                .borrow()
                                .get(parameter_id)
                                .unwrap()
                                .name
                        )?;
                    }
                    writeln!(w, "{}\"];", node.shape)?;
                }
            }
            if kernel_output == KernelDotOutput::Cluster && cluster_id.is_some() {
                writeln!(w, "}}")?;
            }
        }
        for edge_ref in self.ops.edge_references() {
            write!(
                w,
                "n{} -> n{}",
                edge_ref.source().index(),
                edge_ref.target().index()
            )?;
            let mut label = String::new();
            if self.ops[edge_ref.target()]
                .op
                .is_gather_arg(edge_ref.weight().arg)
            {
                label.push('G');
            }
            if !edge_ref.weight().view.is_contiguous() {
                label.push('V')
            }
            if !label.is_empty() {
                write!(w, " [label=\"{}\"]", label)?;
            }
            writeln!(w, ";")?;
        }
        writeln!(w, "}}")
    }
}
