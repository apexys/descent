use crate::common::*;
use ordered_float::NotNan;
use petgraph::Incoming;
use slotmap::SparseSecondaryMap;
use std::{cell::RefCell, ops};

#[derive(Clone, Copy)]
pub struct Array<'builder> {
    node_id: OpNodeId,
    builder: &'builder GraphBuilder,
}

#[derive(Clone, Copy)]
pub struct DualArray<'builder> {
    node_ids: DualOpNodeId,
    builder: &'builder GraphBuilder,
}

impl<'builder> Array<'builder> {
    pub fn graph(&self) -> &'builder GraphBuilder {
        self.builder
    }

    fn broadcast(self, shape: &Shape) -> Self {
        self.builder.with_state(|state| {
            let self_shape = state.ops.graph[self.node_id].shape.clone();
            if &self_shape == shape {
                self
            } else {
                Array {
                    node_id: state.ops.new_node(
                        shape.clone(),
                        Op::View(View::broadcast(&self_shape, &shape)),
                        &[self.node_id],
                    ),
                    builder: self.builder,
                }
            }
        })
    }

    fn unary_op(self, op: UnaryOp) -> Self {
        self.builder.with_state(|state| {
            let shape = state.ops.graph[self.node_id].shape.clone();
            Array {
                node_id: state.ops.new_node(shape, Op::Unary(op), &[self.node_id]),
                builder: self.builder,
            }
        })
    }

    fn binary_op(self, rhs: Array, op: BinaryOp) -> Self {
        let op_shape = self.builder.with_state(|state| {
            state.ops.graph[self.node_id]
                .shape
                .match_with_broadcast(&state.ops.graph[rhs.node_id].shape)
        });

        let lhs = self.broadcast(&op_shape).node_id;
        let rhs = rhs.broadcast(&op_shape).node_id;

        self.builder.with_state(|state| Array {
            node_id: state.ops.new_node(op_shape, Op::Binary(op), &[lhs, rhs]),
            builder: self.builder,
        })
    }

    fn compare_and_select(
        self,
        compare_mode: CompareMode,
        rhs: Array,
        pass: Array,
        fail: Array,
    ) -> Self {
        let op_shape = self.builder.with_state(|state| {
            state.ops.graph[self.node_id]
                .shape
                .match_with_broadcast(&state.ops.graph[rhs.node_id].shape)
                .match_with_broadcast(&state.ops.graph[pass.node_id].shape)
                .match_with_broadcast(&state.ops.graph[fail.node_id].shape)
        });

        let lhs = self.broadcast(&op_shape).node_id;
        let rhs = rhs.broadcast(&op_shape).node_id;
        let pass = pass.broadcast(&op_shape).node_id;
        let fail = fail.broadcast(&op_shape).node_id;

        self.builder.with_state(|state| Array {
            node_id: state.ops.new_node(
                op_shape,
                Op::CompareAndSelect(compare_mode),
                &[lhs, rhs, pass, fail],
            ),
            builder: self.builder,
        })
    }

    fn reduce_op(self, reduce_op: ReduceOp, axis: Axis) -> Self {
        self.builder.with_state(|state| {
            let shape = state.ops.graph[self.node_id].shape.reduce(axis);
            Array {
                node_id: state
                    .ops
                    .new_node(shape, Op::Reduce { reduce_op, axis }, &[self.node_id]),
                builder: self.builder,
            }
        })
    }

    pub fn one_hot(self, count: usize) -> Self {
        self.builder.with_state(|state| {
            let shape = state.ops.graph[self.node_id].shape.one_hot(count);
            Array {
                node_id: state
                    .ops
                    .new_node(shape, Op::Unary(UnaryOp::OneHot), &[self.node_id]),
                builder: self.builder,
            }
        })
    }

    pub fn reduce_max(self, axis: isize) -> Self {
        self.reduce_op(ReduceOp::Max, self.shape().axis(axis))
    }
    pub fn reduce_sum(self, axis: isize) -> Self {
        self.reduce_op(ReduceOp::Sum, self.shape().axis(axis))
    }

    pub fn argmax(self, axis: isize) -> Self {
        // implement with reduce_max for now
        let coord_or_zero = self.select_eq(
            self.reduce_max(axis),
            self.coord(axis),
            self.builder.literal(0.0),
        );
        coord_or_zero.reduce_max(axis)
    }

    fn reduce_onto_per_element(self, shape: &Shape) -> Self {
        let mut output = self;
        while let Some(axis) = output.shape().reduce_axis_onto_per_element(shape) {
            output = output.reduce_op(ReduceOp::Sum, axis);
        }
        output
    }

    pub fn coord(self, axis: isize) -> Self {
        self.builder.coord(self.shape(), axis)
    }

    pub fn select_eq(self, rhs: Array, pass: Array, fail: Array) -> Self {
        self.compare_and_select(CompareMode::Eq, rhs, pass, fail)
    }
    pub fn select_gt(self, rhs: Array, pass: Array, fail: Array) -> Self {
        self.compare_and_select(CompareMode::Gt, rhs, pass, fail)
    }

    pub fn sqrt(self) -> Self {
        self.unary_op(UnaryOp::Sqrt)
    }
    pub fn exp(self) -> Self {
        self.unary_op(UnaryOp::Exp)
    }
    pub fn log(self) -> Self {
        self.unary_op(UnaryOp::Log)
    }

    pub fn matmul(self, rhs: Array) -> Self {
        self.builder.with_state(|state| {
            let shape = state.ops.graph[self.node_id]
                .shape
                .matrix_multiply(&state.ops.graph[rhs.node_id].shape);
            Array {
                node_id: state
                    .ops
                    .new_node(shape, Op::MatMul, &[self.node_id, rhs.node_id]),
                builder: self.builder,
            }
        })
    }

    pub fn transpose(self) -> Self {
        self.builder.with_state(|state| {
            let input_shape = &state.ops.graph[self.node_id].shape;
            let view = input_shape.identity_view().transposed();
            let output_shape = input_shape.transposed();
            Array {
                node_id: state
                    .ops
                    .new_node(output_shape, Op::View(view), &[self.node_id]),
                builder: self.builder,
            }
        })
    }

    pub fn shape(&self) -> Shape {
        self.builder
            .with_state(|state| state.ops.graph[self.node_id].shape.clone())
    }

    pub fn accumulate(&self, src: Array) {
        self.builder.with_state(|state| {
            assert_eq!(state.ops.graph[self.node_id].op, Op::Accumulate);
            assert_eq!(
                state.ops.graph[self.node_id].shape,
                state.ops.graph[src.node_id].shape
            );
            let arg = state
                .ops
                .graph
                .edges_directed(self.node_id, Incoming)
                .count();
            state.ops.graph.add_edge(
                src.node_id,
                self.node_id,
                OpEdge {
                    arg,
                    view: state.ops.graph[src.node_id].shape.identity_view(),
                },
            );
        })
    }
}

impl<'builder> ops::Add for Array<'builder> {
    type Output = Array<'builder>;
    fn add(self, rhs: Array) -> Self::Output {
        self.binary_op(rhs, BinaryOp::Add)
    }
}
impl<'builder> ops::Add<f32> for Array<'builder> {
    type Output = Array<'builder>;
    fn add(self, rhs: f32) -> Self::Output {
        let rhs = self.builder.literal(rhs);
        self.binary_op(rhs, BinaryOp::Add)
    }
}

impl<'builder> ops::Sub for Array<'builder> {
    type Output = Array<'builder>;
    fn sub(self, rhs: Array) -> Self::Output {
        self.binary_op(rhs, BinaryOp::Sub)
    }
}
impl<'builder> ops::Sub<Array<'builder>> for f32 {
    type Output = Array<'builder>;
    fn sub(self, rhs: Array<'builder>) -> Self::Output {
        let lhs = rhs.builder.literal(self);
        lhs.binary_op(rhs, BinaryOp::Sub)
    }
}

impl<'builder> ops::Mul for Array<'builder> {
    type Output = Array<'builder>;
    fn mul(self, rhs: Array) -> Self::Output {
        self.binary_op(rhs, BinaryOp::Mul)
    }
}
impl<'builder> ops::Mul<f32> for Array<'builder> {
    type Output = Array<'builder>;
    fn mul(self, rhs: f32) -> Self::Output {
        let rhs = self.builder.literal(rhs);
        self.binary_op(rhs, BinaryOp::Mul)
    }
}

impl<'builder> ops::Div for Array<'builder> {
    type Output = Array<'builder>;
    fn div(self, rhs: Array) -> Self::Output {
        self.binary_op(rhs, BinaryOp::Div)
    }
}
impl<'builder> ops::Neg for Array<'builder> {
    type Output = Array<'builder>;
    fn neg(self) -> Self::Output {
        self.unary_op(UnaryOp::Neg)
    }
}

impl<'builder> ops::Mul<Array<'builder>> for f32 {
    type Output = Array<'builder>;
    fn mul(self, rhs: Array<'builder>) -> Self::Output {
        let lhs = rhs.builder.literal(self);
        lhs.binary_op(rhs, BinaryOp::Mul)
    }
}
impl<'builder> ops::Div<f32> for Array<'builder> {
    type Output = Array<'builder>;
    fn div(self, rhs: f32) -> Self::Output {
        let rhs = self.builder.literal(rhs);
        self.binary_op(rhs, BinaryOp::Div)
    }
}

impl<'builder> DualArray<'builder> {
    pub fn new(value: Array<'builder>, grad: Array<'builder>) -> Self {
        Self {
            node_ids: DualOpNodeId {
                value: value.node_id,
                grad: grad.node_id,
            },
            builder: value.builder,
        }
    }

    pub fn value(self) -> Array<'builder> {
        Array {
            node_id: self.node_ids.value,
            builder: self.builder,
        }
    }

    pub fn grad(self) -> Array<'builder> {
        Array {
            node_id: self.node_ids.grad,
            builder: self.builder,
        }
    }

    pub fn shape(&self) -> Shape {
        self.builder
            .with_state(|state| state.ops.graph[self.node_ids.value].shape.clone())
    }

    pub fn graph(&self) -> &'builder GraphBuilder {
        self.builder
    }

    pub fn matmul(self, rhs: DualArray) -> Self {
        let a = self.value();
        let da = self.grad();
        let b = rhs.value();
        let db = rhs.grad();

        let c = a.matmul(b);
        let dc = self.builder.accumulator(c.shape());

        da.accumulate(dc.matmul(b.transpose()));
        db.accumulate(a.transpose().matmul(dc));

        Self::new(c, dc)
    }
}

impl<'builder> ops::Add for DualArray<'builder> {
    type Output = DualArray<'builder>;
    fn add(self, rhs: DualArray<'builder>) -> Self::Output {
        let a = self.value();
        let da = self.grad();
        let b = rhs.value();
        let db = rhs.grad();

        let c = a + b;
        let dc = self.builder.accumulator(c.shape());

        da.accumulate(dc.reduce_onto_per_element(&a.shape()));
        db.accumulate(dc.reduce_onto_per_element(&b.shape()));

        Self::new(c, dc)
    }
}

struct OpGraphBuilder {
    graph: OpGraph,
    colour: usize,
}

impl OpGraphBuilder {
    fn new_node(&mut self, shape: impl Into<Shape>, op: Op, inputs: &[OpNodeId]) -> OpNodeId {
        let shape = shape.into();
        let node_id = self.graph.add_node(OpNode {
            colour: self.colour,
            shape,
            op,
            cluster_id: None,
        });
        for (index, input_id) in inputs.iter().copied().enumerate() {
            self.graph.add_edge(
                input_id,
                node_id,
                OpEdge {
                    arg: index,
                    view: self.graph[input_id].shape.identity_view(),
                },
            );
        }
        node_id
    }
}

struct GraphBuilderState {
    ops: OpGraphBuilder,
    variables: SharedVariables,
    inputs: SparseSecondaryMap<VariableId, DualOpNodeId>,
    outputs: SparseSecondaryMap<VariableId, OpNodeId>,
}

pub struct GraphBuilder {
    state: RefCell<GraphBuilderState>,
}

impl GraphBuilder {
    pub(crate) fn new(variables: SharedVariables) -> Self {
        Self {
            state: RefCell::new(GraphBuilderState {
                ops: OpGraphBuilder {
                    graph: Default::default(),
                    colour: 0,
                },
                variables,
                inputs: SparseSecondaryMap::new(),
                outputs: SparseSecondaryMap::new(),
            }),
        }
    }

    fn with_state<F, T>(&self, f: F) -> T
    where
        F: FnOnce(&mut GraphBuilderState) -> T,
    {
        let mut data = self.state.borrow_mut();
        f(&mut data)
    }

    pub fn literal(&self, value: f32) -> Array {
        self.with_state(|state| Array {
            node_id: state
                .ops
                .new_node([1], Op::Literal(NotNan::new(value).unwrap()), &[]),
            builder: self,
        })
    }

    pub fn coord(&self, shape: impl Into<Shape>, axis: isize) -> Array {
        self.with_state(|state| {
            let shape = shape.into();
            let axis = shape.axis(axis);
            Array {
                node_id: state
                    .ops
                    .new_node(shape, Op::BuiltIn(BuiltInOp::Coord { axis }), &[]),
                builder: self,
            }
        })
    }

    pub fn input(&self, variable_id: VariableId) -> DualArray {
        let node_ids = self.with_state(|state| {
            let shape = state
                .variables
                .borrow()
                .get(variable_id)
                .unwrap()
                .shape
                .clone();
            let ops = &mut state.ops;
            *state
                .inputs
                .entry(variable_id)
                .unwrap()
                .or_insert_with(|| DualOpNodeId {
                    value: ops.new_node(shape.clone(), Op::Input { variable_id }, &[]),
                    grad: ops.new_node(shape, Op::Accumulate, &[]),
                })
        });
        DualArray {
            node_ids,
            builder: self,
        }
    }

    pub fn output(&self, variable_id: VariableId, rhs: Array) {
        self.with_state(|state| {
            let shape = state.ops.graph[rhs.node_id].shape.clone();
            assert_eq!(
                state
                    .variables
                    .borrow()
                    .get(variable_id)
                    .unwrap()
                    .shape
                    .clone(),
                shape
            );

            // update the output node for this variable (remove any old one)
            let node_id =
                state
                    .ops
                    .new_node(shape.clone(), Op::Output { variable_id }, &[rhs.node_id]);
            if let Some(node_id) = state.outputs.insert(variable_id, node_id) {
                state.ops.graph.remove_node(node_id);
            }

            // ensure that if we read this variable again we read the latest value
            state.inputs.insert(
                variable_id,
                DualOpNodeId {
                    value: rhs.node_id,
                    grad: state.ops.new_node(shape, Op::Accumulate, &[]),
                },
            );
        });
    }

    pub fn accumulator(&self, shape: impl Into<Shape>) -> Array {
        self.with_state(|state| Array {
            node_id: state.ops.new_node(shape, Op::Accumulate, &[]),
            builder: self,
        })
    }

    pub fn next_colour(&self) {
        self.with_state(|state| {
            state.ops.colour += 1;
        })
    }

    pub fn build(self) -> Graph {
        self.with_state(|state| {
            Graph::new(
                SharedVariables::clone(&state.variables),
                state.ops.graph.clone(),
            )
        })
    }
}
