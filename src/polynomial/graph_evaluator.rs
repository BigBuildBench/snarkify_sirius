use halo2_proofs::poly::Rotation;
use tracing::*;

use super::Expression;
/// This module provides an efficient and flexible way to evaluate expressions that represent
/// can be represented as a graph of calculations.
///
/// At its core, the evaluator implements an algorithm that recursively breaks down expressions into
/// a sequence of simpler calculations. These calculations are then organized into a linear vector,
/// which the evaluator processes to compute the final result. The algorithm makes extensive use of
/// intermediate values to optimize and speed up the computation, especially for expressions that
/// involve repeated sub-expressions.
///
/// ## Algorithm Overview
///
/// 1. **Recursive Decomposition**: The evaluator starts by recursively decomposing a given
///    expression into simpler sub-expressions.
///    [`GraphEvaluator::new`] with [`GraphEvaluator::add_expression`]
///
/// 2. **Building Calculation Nodes**: As it decomposes the expressions, the evaluator constructs
///    "calculation nodes" for each operation. Each node is a representation of a calculation to be
///    performed and is stored in a linear vector in the order they are encountered.
///    [`GraphEvaluator::add_calculation`]
///
/// 3. **Handling Value Sources**: The evaluator identifies the source of each value involved in a
///    calculation. Value sources can be constants, intermediates, fixed or advice columns, or
///    challenges. The evaluator assigns an index to each unique source, facilitating efficient
///    retrieval during the evaluation phase.
///    [`Calculation::evaluate`]
///
/// 4. **Optimizing with Intermediate Values**: The algorithm optimizes the calculations by reusing
///    intermediate values. If a sub-expression occurs multiple times within the larger expression,
///    its result is calculated once and stored as an intermediate value. Subsequent occurrences of
///    the same sub-expression then simply reuse the stored intermediate value, avoiding redundant
///    calculations.
///
/// 5. **Evaluation**: Once all calculation nodes are established, the evaluator sequentially
///    processes the vector of calculations. It performs each calculation using the values
///    retrieved based on their sources, including any intermediate values generated by earlier
///    calculations. The result of the final calculation is the result of the entire expression.
///    [`GraphEvaluator::evaluate`]
///
/// ## References
///
/// It is an adaptation for our needs of the [code from
/// halo2](https://github.com/privacy-scaling-explorations/halo2/blob/main/halo2_backend/src/plonk/evaluation.rs#L200)
use crate::ff::PrimeField;
use crate::plonk::eval::{Error as EvalError, GetDataForEval};

/// Return the index in the polynomial of size `isize` after rotation `rot`.
fn get_rotation_idx(idx: usize, rot: i32, num_row: usize) -> usize {
    (((idx as i32) + rot).rem_euclid(num_row as i32)) as usize
}

/// Value used in a calculation
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd)]
enum ValueSource {
    /// This is a constant value
    Constant(usize),
    /// This is an intermediate value
    Intermediate(usize),
    /// This is a fixed column
    Fixed { index: usize, rotation: usize },
    /// This is an advice (witness) column
    Poly { index: usize, rotation: usize },
    /// This is a challenge
    Challenge { index: usize },
}

/// Calculation
#[derive(Clone, Debug, PartialEq, Eq)]
enum Calculation {
    /// This is an addition
    Add(ValueSource, ValueSource),
    /// This is a subtraction
    Sub(ValueSource, ValueSource),
    /// This is a product
    Mul(ValueSource, ValueSource),
    /// This is a square
    Square(ValueSource),
    /// This is a double
    Double(ValueSource),
    /// This is a negation
    Negate(ValueSource),
    /// This is Horner's rule: `val = a; val = val * c + b[]`
    Horner(ValueSource, Vec<ValueSource>, ValueSource),
    /// This is a simple assignment
    Store(ValueSource),
}

impl Calculation {
    /// Get the resulting value of this calculation
    fn evaluate<F: PrimeField>(
        &self,
        rotations: &[usize],
        constants: &[F],
        intermediates: &[F],
        eval_getter: &impl GetDataForEval<F>,
    ) -> Result<F, EvalError> {
        let get_value = |value: &ValueSource| -> Result<F, EvalError> {
            match value {
                ValueSource::Constant(id) => Ok(constants[*id]),
                ValueSource::Intermediate(id) => Ok(intermediates[*id]),
                ValueSource::Fixed { index, rotation } => eval_getter
                    .get_fixed()
                    .as_ref()
                    .get(*index)
                    .ok_or(EvalError::ColumnVariableIndexOutOfBoundary {
                        column_index: *index,
                    })?
                    .get(rotations[*rotation])
                    .cloned()
                    .ok_or(EvalError::RowIndexOutOfBoundary {
                        row_index: rotations[*rotation],
                    }),
                ValueSource::Poly { index, rotation } => {
                    Ok(eval_getter.eval_column_var(rotations[*rotation], *index)?)
                }
                ValueSource::Challenge { index } => {
                    let challenges = eval_getter.get_challenges().as_ref();
                    challenges
                        .get(*index)
                        .cloned()
                        .ok_or(EvalError::ChallengeIndexOutOfBoundary {
                            challenge_index: *index,
                            challeges_len: challenges.len(),
                        })
                }
            }
        };

        Ok(match self {
            Calculation::Add(a, b) => get_value(a)? + get_value(b)?,
            Calculation::Sub(a, b) => get_value(a)? - get_value(b)?,
            Calculation::Mul(a, b) => get_value(a)? * get_value(b)?,
            Calculation::Square(v) => get_value(v)?.square(),
            Calculation::Double(v) => get_value(v)?.double(),
            Calculation::Negate(v) => -get_value(v)?,
            Calculation::Horner(start_value, parts, factor) => {
                let factor = get_value(factor)?;
                let mut value = get_value(start_value)?;
                for part in parts.iter() {
                    value = value * factor + get_value(part)?;
                }
                value
            }
            Calculation::Store(v) => get_value(v)?,
        })
    }
}

#[derive(Clone, Debug)]
struct CalculationInfo {
    calculation: Calculation,
    target: usize,
}

#[derive(Default, Debug)]
struct EvaluationData<F: PrimeField> {
    intermediates: Vec<F>,
    rotations: Vec<usize>,
}

/// Allows you to calculate [`Expression`] values based on the data provided by [`GetDataForEval`]
#[derive(Clone, Debug)]
pub struct GraphEvaluator<F: PrimeField> {
    /// Constant values used during the calculation. They are accessed by index during execution.
    /// To avoid storing the same `Scalar` values in multiple nodes.
    ///
    /// TODO #159 Consider better ways of storage (sorted for example)
    constants: Vec<F>,
    rotations: Vec<i32>,
    num_intermediates: usize,
    /// All calculations to be performed within the graph
    ///
    /// Arranged in such an order that if some calculation requires another calculation, the latter
    /// will be at a lower index. This allows the nodes of calculations to be arranged linearly and
    /// is provided by recursion.
    calculations: Vec<CalculationInfo>,
}

impl<F: PrimeField> Default for GraphEvaluator<F> {
    fn default() -> Self {
        Self {
            // The most used constants are added here, for simplicity's sake
            constants: vec![F::ZERO, F::ONE, F::from(2u64)],
            rotations: Default::default(),
            calculations: Default::default(),
            num_intermediates: Default::default(),
        }
    }
}

impl<F: PrimeField> GraphEvaluator<F> {
    #[instrument(name = "graph_evaluator_new", skip_all, level = Level::DEBUG)]
    pub fn new(expr: &Expression<F>) -> Self {
        let mut self_ = GraphEvaluator::default();

        let value_source = self_.add_expression(expr);
        self_.add_calculation(Calculation::Store(value_source));

        self_
    }

    /// Adds a rotation
    fn add_rotation(&mut self, rotation: &Rotation) -> usize {
        match self.rotations.iter().position(|&c| c == rotation.0) {
            Some(index) => {
                debug!("rotation {rotation:?} already have index: {index}, will use it");
                index
            }
            None => {
                self.rotations.push(rotation.0);
                let index = self.rotations.len() - 1;
                debug!("rotation {rotation:?} have't index, add it with index: {index}");
                index
            }
        }
    }

    /// Adds a constant
    fn add_constant(&mut self, constant: &F) -> ValueSource {
        ValueSource::Constant(match self.constants.iter().position(|&c| c == *constant) {
            Some(index) => {
                debug!("constant {constant:?} already have index: {index}, will use it");
                index
            }
            None => {
                self.constants.push(*constant);
                let index = self.constants.len() - 1;
                debug!("constant {constant:?} have't index, add it with index: {index}");
                index
            }
        })
    }

    /// Adds a calculation.
    /// Currently does the simplest thing possible: just stores the
    /// resulting value so the result can be reused  when that calculation
    /// is done multiple times.
    fn add_calculation(&mut self, calculation: Calculation) -> ValueSource {
        let existing_calculation = self
            .calculations
            .iter()
            .find(|c| c.calculation == calculation);
        match existing_calculation {
            Some(existing_calculation) => ValueSource::Intermediate(existing_calculation.target),
            None => {
                let target = self.num_intermediates;
                self.calculations.push(CalculationInfo {
                    calculation,
                    target,
                });
                self.num_intermediates += 1;
                ValueSource::Intermediate(target)
            }
        }
    }

    /// Generates an optimized evaluation for the expression
    fn add_expression(&mut self, expr: &Expression<F>) -> ValueSource {
        match expr {
            Expression::Constant(scalar) => self.add_constant(scalar),
            Expression::Polynomial(query) => {
                let rot_idx = self.add_rotation(&query.rotation);
                self.add_calculation(Calculation::Store(ValueSource::Poly {
                    index: query.index,
                    rotation: rot_idx,
                }))
            }
            Expression::Challenge(challenge_index) => {
                self.add_calculation(Calculation::Store(ValueSource::Challenge {
                    index: *challenge_index,
                }))
            }
            Expression::Negated(a) => match **a {
                Expression::Constant(scalar) => self.add_constant(&-scalar),
                _ => {
                    let result_a = self.add_expression(a);
                    match result_a {
                        ValueSource::Constant(0) => result_a,
                        _ => self.add_calculation(Calculation::Negate(result_a)),
                    }
                }
            },
            Expression::Sum(a, b) => {
                // Undo subtraction stored as a + (-b) in expressions
                match &**b {
                    Expression::Negated(b_int) => {
                        let result_a = self.add_expression(a);
                        let result_b = self.add_expression(b_int);
                        if result_a == ValueSource::Constant(0) {
                            self.add_calculation(Calculation::Negate(result_b))
                        } else if result_b == ValueSource::Constant(0) {
                            result_a
                        } else {
                            self.add_calculation(Calculation::Sub(result_a, result_b))
                        }
                    }
                    _ => {
                        let expr_a_value_source = self.add_expression(a);
                        let expr_b_value_source = self.add_expression(b);

                        if expr_a_value_source <= expr_b_value_source {
                            self.add_calculation(Calculation::Add(
                                expr_a_value_source,
                                expr_b_value_source,
                            ))
                        } else {
                            self.add_calculation(Calculation::Add(
                                expr_b_value_source,
                                expr_a_value_source,
                            ))
                        }
                    }
                }
            }
            Expression::Product(a, b) => {
                let result_a = self.add_expression(a);
                let result_b = self.add_expression(b);
                if result_a == ValueSource::Constant(0) || result_b == ValueSource::Constant(0) {
                    ValueSource::Constant(0)
                } else if result_a == ValueSource::Constant(1) {
                    result_b
                } else if result_b == ValueSource::Constant(1) {
                    result_a
                } else if result_a == ValueSource::Constant(2) {
                    self.add_calculation(Calculation::Double(result_b))
                } else if result_b == ValueSource::Constant(2) {
                    self.add_calculation(Calculation::Double(result_a))
                } else if result_a == result_b {
                    self.add_calculation(Calculation::Square(result_a))
                } else if result_a <= result_b {
                    self.add_calculation(Calculation::Mul(result_a, result_b))
                } else {
                    self.add_calculation(Calculation::Mul(result_b, result_a))
                }
            }
            Expression::Scaled(a, f) => {
                if *f == F::ZERO {
                    ValueSource::Constant(0)
                } else if *f == F::ONE {
                    self.add_expression(a)
                } else {
                    let cst = self.add_constant(f);
                    let result_a = self.add_expression(a);
                    self.add_calculation(Calculation::Mul(result_a, cst))
                }
            }
        }
    }

    /// Creates a new evaluation structure
    fn instance(&self) -> EvaluationData<F> {
        EvaluationData {
            intermediates: vec![F::ZERO; self.num_intermediates],
            rotations: vec![0usize; self.rotations.len()],
        }
    }

    pub fn evaluate(
        &self,
        getter: &impl GetDataForEval<F>,
        row_index: usize,
    ) -> Result<F, EvalError> {
        let mut data = self.instance();
        // All rotation index values
        for (rot_idx, rot) in self.rotations.iter().enumerate() {
            data.rotations[rot_idx] = get_rotation_idx(row_index, *rot, getter.row_size());
        }

        // All calculations, with cached intermediate results
        for calc in self.calculations.iter() {
            data.intermediates[calc.target] = calc.calculation.evaluate(
                &data.rotations,
                &self.constants,
                &data.intermediates,
                getter,
            )?;
        }

        // Return the result of the last calculation (if any)
        if let Some(calc) = self.calculations.last() {
            Ok(data.intermediates[calc.target])
        } else {
            Ok(F::ZERO)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::array;

    use halo2_proofs::halo2curves::CurveAffine;
    use tracing_test::traced_test;

    use super::*;
    use crate::{
        ff::Field,
        halo2curves::bn256,
        plonk::eval::{Error as EvalError, GetDataForEval},
        polynomial::Query,
    };

    #[derive(Default)]
    struct Mock<F: PrimeField> {
        challenges: Vec<F>,
        selectors: Vec<Vec<bool>>,
        fixed: Vec<Vec<F>>,
        advice: Vec<Vec<F>>,
        num_lookup: usize,
    }

    impl<F: PrimeField> GetDataForEval<F> for Mock<F> {
        fn get_challenges(&self) -> &impl AsRef<[F]> {
            &self.challenges
        }

        fn get_selectors(&self) -> &impl AsRef<[Vec<bool>]> {
            &self.selectors
        }

        fn get_fixed(&self) -> &impl AsRef<[Vec<F>]> {
            &self.fixed
        }

        fn eval_advice_var(&self, row_index: usize, column_index: usize) -> Result<F, EvalError> {
            self.advice
                .get(column_index)
                .ok_or(EvalError::ColumnVariableIndexOutOfBoundary { column_index })
                .and_then(|column| {
                    column
                        .get(row_index)
                        .ok_or(EvalError::RowIndexOutOfBoundary { row_index })
                })
                .cloned()
        }

        fn num_lookup(&self) -> usize {
            self.num_lookup
        }
    }

    type Scalar = <bn256::G1Affine as CurveAffine>::ScalarExt;

    #[traced_test]
    #[test]
    fn constant() {
        let val = Scalar::random(&mut rand::thread_rng());

        assert_eq!(
            GraphEvaluator::<Scalar>::new(&Expression::Constant(val))
                .evaluate(&Mock::default(), 0)
                .unwrap(),
            val
        );
    }

    #[traced_test]
    #[test]
    fn sum_const() {
        let mut rnd = rand::thread_rng();
        let lhs = Scalar::random(&mut rnd);
        let rhs = Scalar::random(&mut rnd);

        let res = GraphEvaluator::<Scalar>::new(&Expression::Sum(
            Box::new(Expression::Constant(lhs)),
            Box::new(Expression::Constant(rhs)),
        ))
        .evaluate(&Mock::default(), 0)
        .unwrap();

        assert_eq!(res, lhs + rhs);
    }

    #[traced_test]
    #[test]
    fn product_const() {
        let mut rnd = rand::thread_rng();
        let lhs = Scalar::random(&mut rnd);
        let rhs = Scalar::random(&mut rnd);

        let res = GraphEvaluator::<Scalar>::new(&Expression::Product(
            Box::new(Expression::Constant(lhs)),
            Box::new(Expression::Constant(rhs)),
        ))
        .evaluate(&Mock::default(), 0)
        .unwrap();

        assert_eq!(res, lhs * rhs);
    }

    #[traced_test]
    #[test]
    fn neg_const() {
        let value = Scalar::random(&mut rand::thread_rng());

        let res = GraphEvaluator::<Scalar>::new(&Expression::Negated(Box::new(
            Expression::Constant(value),
        )))
        .evaluate(&Mock::default(), 0)
        .unwrap();

        assert_eq!(res, -value);
    }

    #[traced_test]
    #[test]
    fn poly() {
        let mut rnd = rand::thread_rng();
        let [advice00, advice01, advice10, advice11, fixed00, fixed01, fixed10, fixed11] =
            array::from_fn(|_| Scalar::random(&mut rnd));
        let [selector1, selector2] = [true, false];

        let data = Mock {
            advice: vec![vec![advice00, advice10], vec![advice01, advice11]],
            fixed: vec![vec![fixed00, fixed10], vec![fixed01, fixed11]],
            selectors: vec![vec![selector1, selector2], vec![selector1, selector2]],
            ..Default::default()
        };

        let num_selectors = data.num_selectors();
        let num_fixed = data.num_fixed();

        let eval_selector = |column_index, rotation, row| {
            GraphEvaluator::<Scalar>::new(&Expression::Polynomial::<Scalar>(Query {
                index: column_index,
                rotation: Rotation(rotation),
            }))
            .evaluate(&data, row)
        };
        let eval_fixed = |column_index, rotation, row| {
            GraphEvaluator::<Scalar>::new(&Expression::Polynomial::<Scalar>(Query {
                index: num_selectors + column_index,
                rotation: Rotation(rotation),
            }))
            .evaluate(&data, row)
        };
        let eval_advice = |column_index, rotation, row| {
            GraphEvaluator::<Scalar>::new(&Expression::Polynomial::<Scalar>(Query {
                index: num_selectors + num_fixed + column_index,
                rotation: Rotation(rotation),
            }))
            .evaluate(&data, row)
        };

        assert_eq!(eval_advice(0, 0, 0), Ok(advice00));
        assert_eq!(eval_advice(0, 1, 0), Ok(advice10));
        assert_eq!(eval_advice(0, 0, 1), Ok(advice10));
        assert_eq!(eval_advice(0, -1, 1), Ok(advice00));
        assert_eq!(eval_advice(1, 0, 1), Ok(advice11));

        assert_eq!(eval_fixed(0, 0, 0), Ok(fixed00));
        assert_eq!(eval_fixed(0, 0, 1), Ok(fixed10));
        assert_eq!(eval_fixed(0, -1, 1), Ok(fixed00));

        assert_eq!(
            eval_selector(0, 0, 0),
            Ok(if selector1 { Scalar::ONE } else { Scalar::ZERO })
        );

        assert_eq!(
            eval_selector(0, 0, 1),
            Ok(if selector2 { Scalar::ONE } else { Scalar::ZERO })
        );

        assert_eq!(eval_advice(0, 2, 0), Ok(advice00));
        assert_eq!(eval_advice(0, 1, 1), Ok(advice00));
    }

    #[traced_test]
    #[test]
    fn challenge() {
        let value = Scalar::random(&mut rand::thread_rng());

        assert_eq!(
            GraphEvaluator::<Scalar>::new(&Expression::Challenge(0)).evaluate(
                &Mock {
                    challenges: vec![value],
                    ..Default::default()
                },
                0
            ),
            Ok(value)
        );
    }

    #[traced_test]
    #[test]
    fn eval() {
        fn sum(expr: &[Expression<Scalar>]) -> Box<Expression<Scalar>> {
            Box::new(match expr.split_first() {
                Some((first, rest)) => Expression::Sum(Box::new(first.clone()), sum(rest)),
                None => Expression::Constant(Scalar::ZERO),
            })
        }

        let mut rnd = rand::thread_rng();
        let [advice00, advice01, advice10, advice11, fixed00, fixed01, fixed10, fixed11] =
            array::from_fn(|_| Scalar::random(&mut rnd));

        let data = Mock {
            advice: vec![vec![advice00, advice10], vec![advice01, advice11]],
            fixed: vec![vec![fixed00, fixed10], vec![fixed01, fixed11]],
            selectors: vec![vec![false; 2], vec![false; 2]],
            ..Default::default()
        };

        let num_selectors = data.num_selectors();
        let num_fixed = data.num_fixed();

        let get_fixed = |column_index, rotation| {
            Expression::Polynomial::<Scalar>(Query {
                index: num_selectors + column_index,
                rotation: Rotation(rotation),
            })
        };
        let get_advice = |column_index, rotation| {
            Expression::Polynomial::<Scalar>(Query {
                index: num_selectors + num_fixed + column_index,
                rotation: Rotation(rotation),
            })
        };

        assert_eq!(
            GraphEvaluator::<Scalar>::new(&Expression::Product(
                sum(&[get_advice(0, 0), get_advice(1, 0), get_advice(1, 0)]),
                sum(&[get_fixed(0, 0), get_advice(0, 0)]),
            ))
            .evaluate(&data, 0),
            Ok((advice00 + advice01 + advice01) * (fixed00 + advice00))
        );
    }
}
