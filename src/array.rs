use crate::common::{Graph, *};
use ordered_float::NotNan;
use petgraph::prelude::*;
use slotmap::SparseSecondaryMap;
use std::{cell::RefCell, convert::TryInto, ops};
use tinyvec::ArrayVec as TinyVec;

#[derive(Clone, Copy)]
pub struct Array<'s> {
    node_id: OpNodeId,
    scope: &'s Scope,
}

#[derive(Clone, Copy)]
pub struct UArray<'s> {
    node_id: OpNodeId,
    scope: &'s Scope,
}

#[derive(Clone, Copy)]
pub struct DualArray<'s> {
    value_node_id: OpNodeId,
    loss_grad_node_id: OpNodeId,
    scope: &'s Scope,
}

pub trait IntoArray<'s> {
    fn into_array(self, scope: &'s Scope) -> Array<'s>;
}
impl<'s> IntoArray<'s> for Array<'s> {
    fn into_array(self, _scope: &'s Scope) -> Array<'s> {
        self
    }
}
impl<'s> IntoArray<'s> for f32 {
    fn into_array(self, scope: &'s Scope) -> Array<'s> {
        scope.literal(self).value()
    }
}
impl<'s> IntoArray<'s> for &Parameter {
    fn into_array(self, scope: &'s Scope) -> Array<'s> {
        scope.parameter_value(self)
    }
}

pub trait IntoUArray<'s> {
    fn into_array(self, scope: &'s Scope) -> UArray<'s>;
}
impl<'s> IntoUArray<'s> for UArray<'s> {
    fn into_array(self, _scope: &'s Scope) -> UArray<'s> {
        self
    }
}
impl<'s> IntoUArray<'s> for u32 {
    fn into_array(self, scope: &'s Scope) -> UArray<'s> {
        scope.literal_u32(self)
    }
}

pub trait IntoDualArray<'s> {
    fn into_dual_array(self, scope: &'s Scope) -> DualArray<'s>;
}
impl<'s> IntoDualArray<'s> for DualArray<'s> {
    fn into_dual_array(self, _scope: &'s Scope) -> DualArray<'s> {
        self
    }
}
impl<'s> IntoDualArray<'s> for f32 {
    fn into_dual_array(self, scope: &'s Scope) -> DualArray<'s> {
        scope.literal(self)
    }
}
impl<'s> IntoDualArray<'s> for &Parameter {
    fn into_dual_array(self, scope: &'s Scope) -> DualArray<'s> {
        scope.parameter(self)
    }
}

impl<'s> From<(Array<'s>, Array<'s>)> for DualArray<'s> {
    fn from((x, dx): (Array<'s>, Array<'s>)) -> Self {
        DualArray::new(x, dx)
    }
}

pub trait IntoAxis {
    fn into_axis(self, shape: Shape) -> Axis;
}
impl IntoAxis for Axis {
    fn into_axis(self, _shape: Shape) -> Axis {
        self
    }
}
impl IntoAxis for isize {
    fn into_axis(self, shape: Shape) -> Axis {
        shape.axis(self)
    }
}

macro_rules! implement_array_common {
    ($array:ident, $into_array:ident) => {
        impl<'s> $array<'s> {
            pub fn scope(&self) -> &'s Scope {
                self.scope
            }

            fn view(self, view: View) -> Self {
                self.scope.with_state(|state| {
                    let node_id = state.ops.new_node(
                        state.next_colour,
                        view.output_shape,
                        Op::Unary(UnaryOp::Mov),
                        &[],
                    );
                    state
                        .ops
                        .add_edge(self.node_id, node_id, OpEdge { arg: 0, view });
                    $array {
                        node_id,
                        scope: self.scope,
                    }
                })
            }

            pub fn broadcast(self, shape: impl Into<Shape>) -> Self {
                self.view(View::broadcast(self.shape(), shape.into()))
            }

            fn unary_op(self, op: UnaryOp) -> Self {
                self.scope.with_state(|state| {
                    let shape = state.ops[self.node_id].shape;
                    $array {
                        node_id: state.ops.new_node(
                            state.next_colour,
                            shape,
                            Op::Unary(op),
                            &[self.node_id],
                        ),
                        scope: self.scope,
                    }
                })
            }

            fn binary_op(self, rhs: impl $into_array<'s>, op: BinaryOp) -> Self {
                let rhs = rhs.into_array(self.scope);
                let op_shape = self.scope.with_state(|state| {
                    state.ops[self.node_id]
                        .shape
                        .broadcast_with(state.ops[rhs.node_id].shape)
                });

                let lhs = self.broadcast(op_shape).node_id;
                let rhs = rhs.broadcast(op_shape).node_id;

                self.scope.with_state(|state| $array {
                    node_id: state.ops.new_node(
                        state.next_colour,
                        op_shape,
                        Op::Binary(op),
                        &[lhs, rhs],
                    ),
                    scope: self.scope,
                })
            }

            fn keep_axis(self, axis: impl IntoAxis, keep_axis: bool) -> Self {
                if keep_axis {
                    self
                } else {
                    let axis = axis.into_axis(self.shape());
                    self.remove_axis(axis)
                }
            }

            pub(crate) fn remove_axis(self, axis: Axis) -> Self {
                self.reshape(self.shape().remove_axis(axis))
            }

            pub fn limit_axis(
                self,
                axis: impl IntoAxis,
                range: impl ops::RangeBounds<usize>,
            ) -> Self {
                let shape = self.shape();
                let axis = axis.into_axis(shape);
                self.view(View::new_limited(shape, axis, range))
            }

            pub fn lock_axis(self, axis: impl IntoAxis, coord: usize, keep_axis: bool) -> Self {
                let axis = axis.into_axis(self.shape());
                self.limit_axis(axis, coord..=coord)
                    .keep_axis(axis, keep_axis)
            }

            pub fn reshape(self, shape: impl Into<Shape>) -> Self {
                self.scope.with_state(|state| {
                    let shape = shape.into();
                    assert_eq!(
                        state.ops[self.node_id].shape.element_count(),
                        shape.element_count()
                    );
                    $array {
                        node_id: state.ops.new_node(
                            state.next_colour,
                            shape,
                            Op::Unary(UnaryOp::Mov),
                            &[self.node_id],
                        ),
                        scope: self.scope,
                    }
                })
            }

            pub fn transpose(self) -> Self {
                self.view(self.shape().identity_view().transposed())
            }

            pub fn shape(&self) -> Shape {
                self.scope.with_state(|state| state.ops[self.node_id].shape)
            }
        }
    };
}

implement_array_common!(Array, IntoArray);
implement_array_common!(UArray, IntoUArray);

impl<'s> Array<'s> {
    pub fn with_empty_grad(self) -> (Self, Self) {
        let grad = self.scope.with_state(|state| {
            let shape = state.ops[self.node_id].shape;
            Array {
                node_id: state
                    .ops
                    .new_node(state.next_colour, shape, Op::Unary(UnaryOp::Mov), &[]),
                scope: self.scope,
            }
        });
        (self, grad)
    }

    fn unbroadcast(self, shape: Shape) -> Self {
        let mut output = self;

        while output.shape().len() > shape.len() {
            output = output.reduce_sum(0, false);
        }
        assert_eq!(output.shape().len(), shape.len());

        for (index, (source, target)) in output
            .shape()
            .iter()
            .copied()
            .zip(shape.iter().copied())
            .enumerate()
        {
            if source != target {
                assert_eq!(target, 1);
                output = output.reduce_sum(index as isize, true);
            }
        }
        output
    }

    fn compare_and_select(
        self,
        compare_mode: CompareMode,
        rhs: impl IntoArray<'s>,
        pass: impl IntoArray<'s>,
        fail: impl IntoArray<'s>,
    ) -> Self {
        let rhs = rhs.into_array(self.scope);
        let pass = pass.into_array(self.scope);
        let fail = fail.into_array(self.scope);

        let op_shape = self.scope.with_state(|state| {
            state.ops[self.node_id]
                .shape
                .broadcast_with(state.ops[rhs.node_id].shape)
                .broadcast_with(state.ops[pass.node_id].shape)
                .broadcast_with(state.ops[fail.node_id].shape)
        });

        let lhs = self.broadcast(op_shape).node_id;
        let rhs = rhs.broadcast(op_shape).node_id;
        let pass = pass.broadcast(op_shape).node_id;
        let fail = fail.broadcast(op_shape).node_id;

        self.scope.with_state(|state| Array {
            node_id: state.ops.new_node(
                state.next_colour,
                op_shape,
                Op::CompareAndSelect(compare_mode),
                &[lhs, rhs, pass, fail],
            ),
            scope: self.scope,
        })
    }

    pub fn concat(self, other: impl IntoArray<'s>, axis: impl IntoAxis) -> Self {
        let other = other.into_array(self.scope);
        let other_shape = other.shape();

        let shape = self.shape();
        let axis = axis.into_axis(shape);

        let length = shape[axis];
        let other_length = other_shape[axis];
        let total_length = length + other_length;

        let output_shape = shape.resize_axis(axis, total_length);
        assert_eq!(output_shape, other_shape.resize_axis(axis, total_length));
        let output_coord = self
            .scope
            .coord(total_length)
            .value()
            .reshape(output_shape.coord(axis));

        output_coord.compare_and_select(
            CompareMode::Gt,
            (length - 1) as f32,
            other.pad(axis, length, 0),
            self.pad(axis, 0, other_length),
        )
    }

    fn reduce_op(self, reduce_op: ReduceOp, axis: impl IntoAxis) -> Self {
        let shape = self.shape();
        let axis = axis.into_axis(shape);
        if shape[axis] == 1 {
            self
        } else {
            self.scope.with_state(|state| {
                let shape = shape.reduce(axis);
                Array {
                    node_id: state.ops.new_node(
                        state.next_colour,
                        shape,
                        Op::Reduce { reduce_op, axis },
                        &[self.node_id],
                    ),
                    scope: self.scope,
                }
            })
        }
    }

    pub fn one_hot(self, count: usize) -> Self {
        self.scope.coord(count).value().select_eq(self, 1.0, 0.0)
    }

    pub fn reduce_max(self, axis: impl IntoAxis, keep_axis: bool) -> Self {
        let axis = axis.into_axis(self.shape());
        self.reduce_op(ReduceOp::Max, axis)
            .keep_axis(axis, keep_axis)
    }
    pub fn reduce_sum(self, axis: impl IntoAxis, keep_axis: bool) -> Self {
        let axis = axis.into_axis(self.shape());
        self.reduce_op(ReduceOp::Sum, axis)
            .keep_axis(axis, keep_axis)
    }

    pub fn argmax(self, axis: impl IntoAxis, keep_axis: bool) -> Self {
        // implement with reduce_max for now
        let axis = axis.into_axis(self.shape());
        let coord_or_zero = self.select_eq(self.reduce_max(axis, true), self.coord(axis), 0.0);
        coord_or_zero.reduce_max(axis, keep_axis)
    }

    pub fn coord(self, axis: impl IntoAxis) -> Self {
        let shape = self.shape();
        let axis = axis.into_axis(shape);
        let len = shape[axis];
        self.scope.coord(len).value().reshape(shape.coord(axis))
    }

    pub fn gather(self, axis: impl IntoAxis, indices: impl IntoUArray<'s>) -> Self {
        let indices = indices.into_array(self.scope);
        let [index_count]: [usize; 1] = indices.shape().try_into().unwrap();

        let values_shape = self.shape();

        let axis = axis.into_axis(values_shape);
        let shape = values_shape.resize_axis(axis, index_count);
        let index = indices.reshape(shape.coord(axis)).broadcast(shape);

        self.scope.with_state(|state| {
            Array {
                node_id: state.ops.new_node(
                    state.next_colour,
                    shape,
                    Op::Gather { axis },
                    &[self.node_id, index.node_id],
                ),
                scope: self.scope,
            }
        })
    }
    pub fn scatter_add(
        self,
        values: impl IntoArray<'s>,
        axis: impl IntoAxis,
        indices: impl IntoUArray<'s>,
    ) -> Self {
        let shape = self.shape();

        let values = values.into_array(self.scope);
        let values_shape = values.shape();

        let axis = axis.into_axis(shape);

        let indices = indices.into_array(self.scope);
        let [index_count]: [usize; 1] = indices.shape().try_into().unwrap();

        assert_eq!(shape.resize_axis(axis, index_count), values_shape);

        self.scope.with_state(|state| Array {
            node_id: state.ops.new_node(
                state.next_colour,
                shape,
                Op::ScatterAdd { axis },
                &[self.node_id, values.node_id, indices.node_id],
            ),
            scope: self.scope,
        })
    }

    pub fn select_eq(
        self,
        rhs: impl IntoArray<'s>,
        pass: impl IntoArray<'s>,
        fail: impl IntoArray<'s>,
    ) -> Self {
        self.compare_and_select(CompareMode::Eq, rhs, pass, fail)
    }
    pub fn select_gt(
        self,
        rhs: impl IntoArray<'s>,
        pass: impl IntoArray<'s>,
        fail: impl IntoArray<'s>,
    ) -> Self {
        self.compare_and_select(CompareMode::Gt, rhs, pass, fail)
    }

    pub fn square(self) -> Self {
        self * self
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
    pub fn sin(self) -> Self {
        self.unary_op(UnaryOp::Sin)
    }
    pub fn cos(self) -> Self {
        self.unary_op(UnaryOp::Cos)
    }
    pub fn to_u32_bits(self) -> UArray<'s> {
        UArray {
            node_id: self.node_id,
            scope: self.scope,
        }
    }
    pub fn into_u32(self) -> UArray<'s> {
        self.unary_op(UnaryOp::FloatToUint).to_u32_bits()
    }
    pub fn sigmoid(self) -> Self {
        self.exp() / (self.exp() + 1.0)
    }
    pub fn tanh(self) -> Self {
        let a = self.exp();
        let b = (-self).exp();
        (a - b) / (a + b)
    }

    pub fn pow(self, rhs: impl IntoArray<'s>) -> Self {
        self.binary_op(rhs, BinaryOp::Pow)
    }

    pub(crate) fn insert_axis(self, axis: Axis) -> Self {
        self.reshape(self.shape().insert_axis(axis, 1))
    }

    pub(crate) fn permute_axes(self, perm: &[usize]) -> Self {
        self.view(self.shape().identity_view().permute_axes(perm))
    }

    pub fn matmul(self, rhs: impl IntoArray<'s>) -> Self {
        let axis = Axis::from_index(0);
        let lhs = self.insert_axis(axis);
        let rhs = rhs.into_array(self.scope).insert_axis(axis);
        let result = lhs.batched_matmul(rhs, MatMulOutputMode::Batches);
        result.remove_axis(axis)
    }

    pub(crate) fn batched_matmul(self, rhs: Array, output_mode: MatMulOutputMode) -> Self {
        let chunks = self.scope.with_state(|state| {
            let shape = state.ops[self.node_id]
                .shape
                .batched_matmul(state.ops[rhs.node_id].shape, output_mode);
            Array {
                node_id: state.ops.new_node(
                    state.next_colour,
                    shape,
                    Op::MatMul { output_mode },
                    &[self.node_id, rhs.node_id],
                ),
                scope: self.scope,
            }
        });
        let output = chunks.reduce_sum(0, false);
        match output_mode {
            MatMulOutputMode::Batches => output,
            MatMulOutputMode::Rows => output.permute_axes(&[1, 0, 2]),
        }
    }

    pub(crate) fn pad(self, axis: impl IntoAxis, before: usize, after: usize) -> Self {
        if before + after == 0 {
            return self;
        }
        let shape = self.shape();
        let axis = axis.into_axis(shape);
        self.view(shape.padded_view(axis, before, after))
    }

    pub(crate) fn unpad(self, axis: impl IntoAxis, pad: usize) -> Self {
        if pad == 0 {
            return self;
        }
        self.scope.with_state(|state| {
            let shape = state.ops[self.node_id].shape;
            let axis = axis.into_axis(shape);
            let shape = shape.unpad(axis, pad);
            Array {
                node_id: state.ops.new_node(
                    state.next_colour,
                    shape,
                    Op::Unpad { axis, pad },
                    &[self.node_id],
                ),
                scope: self.scope,
            }
        })
    }

    pub(crate) fn pad_image(self, pad: usize) -> Self {
        self.pad(-3, pad, pad).pad(-2, pad, pad)
    }

    pub(crate) fn unpad_image(self, pad: usize) -> Self {
        self.unpad(-3, pad).unpad(-2, pad)
    }

    fn image_to_windows(
        self,
        filter: (usize, usize),
        stride: (usize, usize),
        groups: usize,
    ) -> Self {
        let input_shape = self.shape();
        let in_y_axis = input_shape.axis(-3);
        let in_x_axis = input_shape.axis(-2);
        let in_c_axis = input_shape.axis(-1);

        let mut view = input_shape.identity_view();

        view.output_shape = input_shape.image_to_windows(filter, stride, groups);
        let group_nc = view.output_shape[SignedIndex(-1)];
        let (stride_w, stride_h) = stride;

        view.output_mapping.truncate(view.output_shape.len() - 6);
        view.output_mapping.push(
            input_shape
                .identity_mapping(in_y_axis)
                .stepped(stride_h as isize),
        );
        view.output_mapping.push(
            input_shape
                .identity_mapping(in_x_axis)
                .stepped(stride_w as isize),
        );
        view.output_mapping.push(
            input_shape
                .identity_mapping(in_c_axis)
                .stepped(group_nc as isize),
        );
        view.output_mapping
            .push(input_shape.identity_mapping(in_y_axis));
        view.output_mapping
            .push(input_shape.identity_mapping(in_x_axis));
        view.output_mapping
            .push(input_shape.identity_mapping(in_c_axis));

        self.view(view)
    }

    fn windows_to_image(self, stride: (usize, usize)) -> Self {
        self.scope.with_state(|state| {
            let shape = state.ops[self.node_id].shape.windows_to_image(stride);
            Array {
                node_id: state.ops.new_node(
                    state.next_colour,
                    shape,
                    Op::WindowsToImage { stride },
                    &[self.node_id],
                ),
                scope: self.scope,
            }
        })
    }

    pub fn accumulate(&self, src: impl IntoArray<'s>) {
        let src = src.into_array(self.scope);
        self.scope.with_state(|state| {
            assert_eq!(state.ops[self.node_id].op, Op::Unary(UnaryOp::Mov));
            assert_eq!(state.ops[self.node_id].shape, state.ops[src.node_id].shape);
            let src_id =
                if let Some(edge_ref) = state.ops.edges_directed(self.node_id, Incoming).next() {
                    // remove the edge from the current source to this move
                    let prev_edge_id = edge_ref.id();
                    let prev_src_id = edge_ref.source();
                    state.ops.remove_edge(prev_edge_id);

                    // accumulate with the given array
                    state.ops.new_node(
                        state.next_colour,
                        state.ops[src.node_id].shape,
                        Op::Binary(BinaryOp::Add),
                        &[prev_src_id, src.node_id],
                    )
                } else {
                    src.node_id
                };

            // add the edge to the move
            state.ops.add_edge(
                src_id,
                self.node_id,
                OpEdge {
                    arg: 0,
                    view: state.ops[src.node_id].shape.identity_view(),
                },
            );
        })
    }

    fn set_loss_grad_root(&self) {
        let grad_shape = self.shape();
        let mini_batch_size = grad_shape[0];
        let mini_batch_scale = self
            .scope
            .literal(1.0 / (mini_batch_size as f32))
            .value()
            .broadcast(grad_shape);
        self.scope.with_state(|state| {
            assert_eq!(state.ops[self.node_id].op, Op::Unary(UnaryOp::Mov));
            assert_eq!(state.ops.edges_directed(self.node_id, Incoming).count(), 0);
            state.ops.add_edge(
                mini_batch_scale.node_id,
                self.node_id,
                OpEdge {
                    arg: 0,
                    view: grad_shape.identity_view(),
                },
            );
        })
    }
}

impl<'s> UArray<'s> {
    pub fn to_f32_bits(self) -> Array<'s> {
        Array {
            node_id: self.node_id,
            scope: self.scope,
        }
    }
    pub fn into_f32(self) -> Array<'s> {
        self.unary_op(UnaryOp::UintToFloat).to_f32_bits()
    }
}

macro_rules! implement_arithmetic {
    ($scalar:ident, $array:ident, $into_array:ident, $add:ident, $mul:ident) => {
        impl<'s, T> ops::Add<T> for $array<'s>
        where
            T: $into_array<'s>,
        {
            type Output = $array<'s>;
            fn add(self, rhs: T) -> Self::Output {
                self.binary_op(rhs, BinaryOp::$add)
            }
        }
        impl<'s> ops::Add<$array<'s>> for $scalar {
            type Output = $array<'s>;
            fn add(self, rhs: $array<'s>) -> Self::Output {
                self.into_array(rhs.scope).binary_op(rhs, BinaryOp::$add)
            }
        }

        impl<'s, T> ops::AddAssign<T> for $array<'s>
        where
            T: $into_array<'s>,
        {
            fn add_assign(&mut self, rhs: T) {
                use ops::Add;
                *self = self.add(rhs);
            }
        }

        impl<'s, T> ops::Mul<T> for $array<'s>
        where
            T: $into_array<'s>,
        {
            type Output = $array<'s>;
            fn mul(self, rhs: T) -> Self::Output {
                self.binary_op(rhs, BinaryOp::$mul)
            }
        }
        impl<'s> ops::Mul<$array<'s>> for $scalar {
            type Output = $array<'s>;
            fn mul(self, rhs: $array<'s>) -> Self::Output {
                self.into_array(rhs.scope).binary_op(rhs, BinaryOp::$mul)
            }
        }
    };
}

implement_arithmetic!(f32, Array, IntoArray, Add, Mul);
implement_arithmetic!(u32, UArray, IntoUArray, UAdd, UMul);

impl<'s, T> ops::Sub<T> for Array<'s>
where
    T: IntoArray<'s>,
{
    type Output = Array<'s>;
    fn sub(self, rhs: T) -> Self::Output {
        self.binary_op(rhs, BinaryOp::Sub)
    }
}
impl<'s> ops::Sub<Array<'s>> for f32 {
    type Output = Array<'s>;
    fn sub(self, rhs: Array<'s>) -> Self::Output {
        self.into_array(rhs.scope).binary_op(rhs, BinaryOp::Sub)
    }
}

impl<'s, T> ops::Div<T> for Array<'s>
where
    T: IntoArray<'s>,
{
    type Output = Array<'s>;
    fn div(self, rhs: T) -> Self::Output {
        self.binary_op(rhs, BinaryOp::Div)
    }
}
impl<'s> ops::Div<Array<'s>> for f32 {
    type Output = Array<'s>;
    fn div(self, rhs: Array<'s>) -> Self::Output {
        self.into_array(rhs.scope).binary_op(rhs, BinaryOp::Div)
    }
}

impl<'s> ops::Neg for Array<'s> {
    type Output = Array<'s>;
    fn neg(self) -> Self::Output {
        self.unary_op(UnaryOp::Neg)
    }
}

impl<'s, T> ops::Rem<T> for UArray<'s>
where
    T: IntoUArray<'s>,
{
    type Output = UArray<'s>;
    fn rem(self, rhs: T) -> Self::Output {
        self.binary_op(rhs, BinaryOp::URem)
    }
}
impl<'s, T> ops::BitXor<T> for UArray<'s>
where
    T: IntoUArray<'s>,
{
    type Output = UArray<'s>;
    fn bitxor(self, rhs: T) -> Self::Output {
        self.binary_op(rhs, BinaryOp::UBitXor)
    }
}

impl<'s> DualArray<'s> {
    pub fn new(value: Array<'s>, loss_grad: Array<'s>) -> Self {
        Self {
            value_node_id: value.node_id,
            loss_grad_node_id: loss_grad.node_id,
            scope: value.scope,
        }
    }

    pub fn value(self) -> Array<'s> {
        Array {
            node_id: self.value_node_id,
            scope: self.scope,
        }
    }

    pub fn loss_grad(self) -> Array<'s> {
        Array {
            node_id: self.loss_grad_node_id,
            scope: self.scope,
        }
    }

    pub fn into_inner(self) -> (Array<'s>, Array<'s>) {
        (self.value(), self.loss_grad())
    }

    pub fn shape(&self) -> Shape {
        self.value().shape()
    }

    pub fn scope(&self) -> &'s Scope {
        self.scope
    }

    pub fn square(self) -> Self {
        self * self
    }

    pub fn upsample(self, x_grow_factor: usize, y_grow_factor: usize) -> Self{
        let (a, da) = self.into_inner();
        let input_shape = a.shape();
        assert_eq!(input_shape.len(), 4);
        assert_eq!(a.shape(), da.shape());
        let a_reshaped = a.reshape([input_shape[0], input_shape[1], 1, input_shape[2], 1, input_shape[3]]);
        let da_reshaped = da.reshape([input_shape[0], input_shape[1], 1, input_shape[2], 1, input_shape[3]]);
        let a_broadcasted = a_reshaped.broadcast([input_shape[0], input_shape[1], x_grow_factor, input_shape[2], y_grow_factor, input_shape[3]]);
        let da_broadcasted = da_reshaped.broadcast([input_shape[0], input_shape[1], x_grow_factor, input_shape[2], y_grow_factor, input_shape[3]]);
        let mut output_shape = input_shape;
        output_shape[1] *= x_grow_factor;
        output_shape[2] *= y_grow_factor;
        let a_backshaped = a_broadcasted.reshape(output_shape);
        let da_backshaped = da_broadcasted.reshape(output_shape);
        (a_backshaped, da_backshaped).into()
    }

    pub fn sin(self) -> Self {
        let (a, da) = self.into_inner();

        let (b, db) = a.sin().with_empty_grad();
        da.accumulate(db * a.cos());

        (b, db).into()
    }
    pub fn tanh(self) -> Self {
        let (a, da) = self.into_inner();

        // d/dx tanh(x) = 1 / cosh^2 (x) = 4 / (e^2x + 2 + e^(-2x))
        let (b, db) = a.tanh().with_empty_grad();
        da.accumulate(db * 4.0 / ((2.0 * a).exp() + 2.0 + (-2.0 * a).exp()));

        (b, db).into()
    }
    pub fn sigmoid(self) -> Self {
        let (a, da) = self.into_inner();

        // d/dx e^x / (1 + e^x) = e^x / (1 + e^x)^2
        let (b, db) = a.sigmoid().with_empty_grad();
        da.accumulate(db * a.exp() / (a.exp() + 1.0).square());

        (b, db).into()
    }

    pub fn leaky_relu(self, leakiness: f32) -> Self {
        let (a, da) = self.into_inner();

        let (b, db) = a.select_gt(0.0, a, a * leakiness).with_empty_grad();
        da.accumulate(a.select_gt(0.0, db, db * leakiness));

        (b, db).into()
    }

    pub(crate) fn batched_matmul(self, rhs: DualArray, output_mode: MatMulOutputMode) -> Self {
        let (a, da) = self.into_inner();
        let (b, db) = rhs.into_inner();

        let (c, dc) = a.batched_matmul(b, output_mode).with_empty_grad();
        da.accumulate(dc.batched_matmul(b.transpose(), MatMulOutputMode::Batches));
        db.accumulate(a.transpose().batched_matmul(dc, MatMulOutputMode::Batches));

        (c, dc).into()
    }

    pub fn matmul(self, rhs: impl IntoDualArray<'s>) -> Self {
        let axis = Axis::from_index(0);
        let lhs = self.insert_axis(axis);
        let rhs = rhs.into_dual_array(self.scope).insert_axis(axis);
        let result = lhs.batched_matmul(rhs, MatMulOutputMode::Batches);
        result.remove_axis(axis)
    }

    pub fn transpose(self) -> Self {
        let (a, da) = self.into_inner();

        let (b, db) = a.transpose().with_empty_grad();
        da.accumulate(db.transpose());

        (b, db).into()
    }

    pub fn pow(self, rhs: impl IntoDualArray<'s>) -> Self {
        let (a, da) = self.into_inner();
        let (b, db) = rhs.into_dual_array(self.scope).into_inner();

        // c = a ^ b
        let (c, dc) = a.pow(b).with_empty_grad();
        da.accumulate((dc * b * a.pow(b - 1.0)).unbroadcast(a.shape()));
        db.accumulate((dc * a.log() * c).unbroadcast(b.shape()));

        (c, dc).into()
    }

    pub fn select_eq(
        self,
        rhs: impl IntoDualArray<'s>,
        pass: impl IntoDualArray<'s>,
        fail: impl IntoDualArray<'s>,
    ) -> Self {
        let (a, _da) = self.into_inner();
        let (b, _db) = rhs.into_dual_array(self.scope).into_inner();
        let (pass, dpass) = pass.into_dual_array(self.scope).into_inner();
        let (fail, dfail) = fail.into_dual_array(self.scope).into_inner();

        let (c, dc) = a.select_eq(b, pass, fail).with_empty_grad();
        // TODO: da and db derivative?
        dpass.accumulate(a.select_eq(b, dc, 0.0).unbroadcast(pass.shape()));
        dfail.accumulate(a.select_eq(b, 0.0, dc).unbroadcast(fail.shape()));

        (c, dc).into()
    }

    fn lock_axis_impl(self, axis: Axis, coord: usize) -> Self {
        let (a, da) = self.into_inner();

        let (b, db) = a.lock_axis(axis, coord, true).with_empty_grad();
        da.accumulate(a.coord(axis).select_eq(coord as f32, db, 0.0));

        (b, db).into()
    }

    pub fn lock_axis(self, axis: impl IntoAxis, coord: usize, keep_axis: bool) -> Self {
        let axis = axis.into_axis(self.shape());
        self.lock_axis_impl(axis, coord).keep_axis(axis, keep_axis)
    }

    pub fn reshape(self, shape: impl Into<Shape>) -> Self {
        let old_shape = self.shape();
        let new_shape = shape.into();

        let (a, da) = self.into_inner();

        let (b, db) = a.reshape(new_shape).with_empty_grad();
        da.accumulate(db.reshape(old_shape));

        (b, db).into()
    }

    pub(crate) fn pad_image(self, pad: usize) -> Self {
        let (a, da) = self.into_inner();

        let (b, db) = a.pad_image(pad).with_empty_grad();
        da.accumulate(db.unpad_image(pad));

        (b, db).into()
    }

    pub(crate) fn image_to_windows(
        self,
        filter: (usize, usize),
        stride: (usize, usize),
        groups: usize,
    ) -> Self {
        let (a, da) = self.into_inner();

        let (b, db) = a.image_to_windows(filter, stride, groups).with_empty_grad();
        da.accumulate(db.windows_to_image(stride));

        (b, db).into()
    }

    pub fn next_colour(self) -> Self {
        self.scope().next_colour();
        self
    }

    pub fn map<F>(self, f: F) -> Self
    where
        F: FnOnce(DualArray<'s>) -> DualArray<'s>,
    {
        f(self)
    }

    pub fn conv2d(
        self,
        filter: impl IntoDualArray<'s>,
        pad: usize,
        stride: (usize, usize),
    ) -> Self {
        let filter = filter.into_dual_array(self.scope);

        // pad the input
        let padded = self.pad_image(pad);

        // copy the input into windows that match the filter size
        let padded_shape = padded.shape();
        let filter_shape = filter.shape();
        let [input_m, _input_h, _input_w, input_nc]: [usize; 4] = padded_shape.try_into().unwrap();
        let [filter_g, filter_oc, filter_h, filter_w, filter_ic]: [usize; 5] =
            filter_shape.try_into().unwrap();
        assert_eq!(input_nc, filter_g * filter_ic);
        let windows = padded.image_to_windows((filter_w, filter_h), stride, filter_g);

        // apply the filter using a matrix multiplication
        let windows_shape = windows.shape();
        let [windows_m, output_h, output_w, windows_g, windows_fh, windows_fw, windows_nc]: [usize;
            7] = windows_shape.try_into().unwrap();
        assert_eq!(input_m, windows_m);
        assert_eq!(filter_g, windows_g);
        assert_eq!(filter_h, windows_fh);
        assert_eq!(filter_w, windows_fw);
        assert_eq!(filter_ic, windows_nc);
        let a = windows
            .reshape([
                input_m * output_h * output_w,
                filter_g,
                filter_h * filter_w * filter_ic,
            ])
            .permute_axes(&[1, 0, 2]);
        let b = filter.reshape([filter_g, filter_oc, filter_h * filter_w * filter_ic]);
        let c = a.batched_matmul(b.transpose(), MatMulOutputMode::Rows);

        // reshape output back to 4D
        c.permute_axes(&[1, 0, 2])
            .reshape([input_m, output_h, output_w, filter_g * filter_oc])
    }

    pub fn max_pool2d(self, filter: (usize, usize), stride: (usize, usize)) -> Self {
        let windows = self.image_to_windows(filter, stride, 1);

        let [m, output_h, output_w, groups, filter_h, filter_w, group_nc]: [usize; 7] =
            windows.shape().try_into().unwrap();

        windows
            .reshape([
                m * output_h * output_w * groups,
                filter_h * filter_w,
                group_nc,
            ])
            .reduce_max(1, true)
            .reshape([m, output_h, output_w, groups * group_nc])
    }

    fn reduce_op(self, reduce_op: ReduceOp, axis: Axis) -> Self {
        let (a, da) = self.into_inner();

        let (b, db) = a.reduce_op(reduce_op, axis).with_empty_grad();
        match reduce_op {
            ReduceOp::Max => da.accumulate(a.select_eq(b, db, 0.0)),
            ReduceOp::Sum => da.accumulate(db.broadcast(da.shape())),
        }

        (b, db).into()
    }

    fn insert_axis(self, axis: Axis) -> Self {
        let (a, da) = self.into_inner();

        let (b, db) = a.insert_axis(axis).with_empty_grad();
        da.accumulate(db.remove_axis(axis));

        (b, db).into()
    }

    fn remove_axis(self, axis: Axis) -> Self {
        let (a, da) = self.into_inner();

        let (b, db) = a.remove_axis(axis).with_empty_grad();
        da.accumulate(db.insert_axis(axis));

        (b, db).into()
    }

    fn keep_axis(self, axis: Axis, keep_axis: bool) -> Self {
        if keep_axis {
            self
        } else {
            self.remove_axis(axis)
        }
    }

    pub fn reduce_sum(self, axis: impl IntoAxis, keep_axis: bool) -> Self {
        let axis = axis.into_axis(self.shape());
        self.reduce_op(ReduceOp::Sum, axis)
            .keep_axis(axis, keep_axis)
    }
    pub fn reduce_max(self, axis: impl IntoAxis, keep_axis: bool) -> Self {
        let axis = axis.into_axis(self.shape());
        self.reduce_op(ReduceOp::Max, axis)
            .keep_axis(axis, keep_axis)
    }

    pub fn flatten(self) -> Self {
        let shape = self.shape();
        let (first, suffix) = shape.split_first().unwrap();
        let m = *first;
        let count = suffix.iter().copied().product();
        self.reshape([m, count])
    }

    pub fn set_loss(self) -> Array<'s> {
        self.loss_grad().set_loss_grad_root();
        self.value()
    }

    pub(crate) fn permute_axes(self, perm: &[usize]) -> Self {
        let mut inv_perm: TinyVec<[usize; MAX_DIM]> = TinyVec::new();
        inv_perm.set_len(perm.len());
        for (src, dst) in perm.iter().copied().enumerate() {
            inv_perm[dst] = src;
        }
        assert!(inv_perm
            .iter()
            .copied()
            .enumerate()
            .all(|(dst, src)| perm[src] == dst));

        let (a, da) = self.into_inner();

        let (b, db) = a.permute_axes(perm).with_empty_grad();
        da.accumulate(db.permute_axes(&inv_perm));

        (b, db).into()
    }

    pub fn concat(self, other: impl IntoDualArray<'s>, axis: impl IntoAxis) -> Self {
        let other = other.into_dual_array(self.scope);

        let shape = self.shape();
        let axis = axis.into_axis(shape);
        let length = shape[axis];

        let (a, da) = self.into_inner();
        let (b, db) = other.into_inner();

        let (c, dc) = a.concat(b, axis).with_empty_grad();
        da.accumulate(dc.limit_axis(axis, ..length));
        db.accumulate(dc.limit_axis(axis, length..));

        (c, dc).into()
    }
}

impl<'s, T> ops::Add<T> for DualArray<'s>
where
    T: IntoDualArray<'s>,
{
    type Output = DualArray<'s>;
    fn add(self, rhs: T) -> Self::Output {
        let rhs = rhs.into_dual_array(self.scope);

        let (a, da) = self.into_inner();
        let (b, db) = rhs.into_inner();

        let (c, dc) = (a + b).with_empty_grad();
        da.accumulate(dc.unbroadcast(a.shape()));
        db.accumulate(dc.unbroadcast(b.shape()));

        (c, dc).into()
    }
}

impl<'s, T> ops::AddAssign<T> for DualArray<'s>
where
    T: IntoDualArray<'s>,
{
    fn add_assign(&mut self, rhs: T) {
        use ops::Add;
        *self = self.add(rhs);
    }
}

impl<'s, T> ops::Sub<T> for DualArray<'s>
where
    T: IntoDualArray<'s>,
{
    type Output = DualArray<'s>;
    fn sub(self, rhs: T) -> Self::Output {
        let rhs = rhs.into_dual_array(self.scope);

        let (a, da) = self.into_inner();
        let (b, db) = rhs.into_inner();

        let (c, dc) = (a - b).with_empty_grad();
        da.accumulate(dc.unbroadcast(a.shape()));
        db.accumulate(-dc.unbroadcast(b.shape()));

        (c, dc).into()
    }
}

impl<'s, T> ops::Mul<T> for DualArray<'s>
where
    T: IntoDualArray<'s>,
{
    type Output = DualArray<'s>;
    fn mul(self, rhs: T) -> Self::Output {
        let rhs = rhs.into_dual_array(self.scope);

        let (a, da) = self.into_inner();
        let (b, db) = rhs.into_inner();

        let (c, dc) = (a * b).with_empty_grad();
        da.accumulate((b * dc).unbroadcast(a.shape()));
        db.accumulate((a * dc).unbroadcast(b.shape()));

        (c, dc).into()
    }
}

#[derive(Clone, Copy)]
struct GraphInput {
    value_node_id: OpNodeId,
    grad_node_id: Option<OpNodeId>,
}

struct ScopeState {
    ops: OpGraph,
    next_colour: usize,
    next_rand_uid: usize,
    parameters: SharedParameters,
    inputs: SparseSecondaryMap<ParameterId, GraphInput>,
    outputs: SparseSecondaryMap<ParameterId, OpNodeId>,
}

pub struct Scope {
    state: RefCell<ScopeState>,
}

impl Scope {
    pub(crate) fn new(parameters: SharedParameters) -> Self {
        Self {
            state: RefCell::new(ScopeState {
                ops: Default::default(),
                next_colour: 0,
                next_rand_uid: 0,
                parameters,
                inputs: SparseSecondaryMap::new(),
                outputs: SparseSecondaryMap::new(),
            }),
        }
    }

    fn with_state<F, T>(&self, f: F) -> T
    where
        F: FnOnce(&mut ScopeState) -> T,
    {
        let mut data = self.state.borrow_mut();
        f(&mut data)
    }

    pub fn literal(&self, value: f32) -> DualArray {
        self.with_state(|state| Array {
            node_id: state.ops.new_node(
                state.next_colour,
                [1],
                Op::Literal(Literal::F32(NotNan::new(value).unwrap())),
                &[],
            ),
            scope: self,
        })
        .with_empty_grad()
        .into()
    }

    pub fn literal_u32(&self, value: u32) -> UArray {
        self.with_state(|state| UArray {
            node_id: state.ops.new_node(
                state.next_colour,
                [1],
                Op::Literal(Literal::U32(value)),
                &[],
            ),
            scope: self,
        })
    }

    pub fn coord(&self, len: usize) -> DualArray {
        self.with_state(|state| {
            let shape = Shape::from([len]);
            Array {
                node_id: state.ops.new_node(
                    state.next_colour,
                    shape,
                    Op::BuiltIn(BuiltInOp::Coord),
                    &[],
                ),
                scope: self,
            }
        })
        .with_empty_grad()
        .into()
    }

    pub fn rand(&self, shape: impl Into<Shape>) -> DualArray {
        self.with_state(|state| {
            let shape = shape.into();
            let uid = state.next_rand_uid;
            state.next_rand_uid += 1;
            Array {
                node_id: state.ops.new_node(
                    state.next_colour,
                    shape,
                    Op::BuiltIn(BuiltInOp::Rand { uid }),
                    &[],
                ),
                scope: self,
            }
        })
        .with_empty_grad()
        .into()
    }

    fn input(&self, parameter: &Parameter) -> GraphInput {
        self.with_state(|state| {
            let parameter_id = parameter.checked_id(&state.parameters);
            let shape = state.parameters.borrow().get(parameter_id).unwrap().shape;
            let next_colour = state.next_colour;
            let ops = &mut state.ops;
            *state
                .inputs
                .entry(parameter_id)
                .unwrap()
                .or_insert_with(|| GraphInput {
                    value_node_id: ops.new_node(
                        next_colour,
                        shape,
                        Op::Input { parameter_id },
                        &[],
                    ),
                    grad_node_id: Some(ops.new_node(
                        next_colour,
                        shape,
                        Op::Unary(UnaryOp::Mov),
                        &[],
                    )),
                })
        })
    }

    pub fn parameter(&self, parameter: &Parameter) -> DualArray {
        let input = self.input(parameter);
        DualArray {
            value_node_id: input.value_node_id,
            loss_grad_node_id: input.grad_node_id.unwrap(),
            scope: self,
        }
    }

    pub fn parameter_value(&self, parameter: &Parameter) -> Array {
        let input = self.input(parameter);
        Array {
            node_id: input.value_node_id,
            scope: self,
        }
    }

    pub fn write_parameter_value(&self, parameter: &Parameter, rhs: Array) {
        self.with_state(|state| {
            let parameter_id = parameter.checked_id(&state.parameters);
            let shape = state.ops[rhs.node_id].shape;
            assert_eq!(
                state.parameters.borrow().get(parameter_id).unwrap().shape,
                shape
            );

            // update the output node for this parameter (remove any old one)
            let node_id = state.ops.new_node(
                state.next_colour,
                shape,
                Op::Output { parameter_id },
                &[rhs.node_id],
            );
            if let Some(node_id) = state.outputs.insert(parameter_id, node_id) {
                state.ops.remove_node(node_id);
            }

            // ensure that if we read this parameter again we read the latest value
            state.inputs.insert(
                parameter_id,
                GraphInput {
                    value_node_id: rhs.node_id,
                    grad_node_id: None,
                },
            );
        });
    }

    pub fn update_parameter_value<'s>(
        &'s self,
        parameter: &Parameter,
        f: impl FnOnce(Array<'s>) -> Array<'s>,
    ) -> Array<'s> {
        let result = f(self.parameter_value(parameter));
        self.write_parameter_value(parameter, result);
        result
    }

    pub fn accumulator(&self, shape: impl Into<Shape>) -> Array {
        self.with_state(|state| Array {
            node_id: state
                .ops
                .new_node(state.next_colour, shape, Op::Unary(UnaryOp::Mov), &[]),
            scope: self,
        })
    }

    pub fn next_colour(&self) {
        self.with_state(|state| {
            state.next_colour += 1;
        })
    }

    pub fn trainable_parameters(&self) -> Vec<Parameter> {
        self.with_state(|state| {
            let mut v = Vec::new();
            for node in state.ops.node_weights() {
                if let Op::Input { parameter_id } = node.op {
                    let parameter = Parameter::new(parameter_id, &state.parameters);
                    if parameter.is_trainable() {
                        v.push(parameter);
                    }
                }
            }
            v
        })
    }

    pub fn build_graph(self) -> Graph {
        self.with_state(|state| {
            Graph::new(
                SharedParameters::clone(&state.parameters),
                state.ops.clone(),
            )
        })
    }
}
