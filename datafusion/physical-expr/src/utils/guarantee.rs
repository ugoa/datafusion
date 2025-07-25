// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

//! [`LiteralGuarantee`] predicate analysis to determine if a column is a
//! constant.

use crate::utils::split_disjunction;
use crate::{split_conjunction, PhysicalExpr};
use datafusion_common::{Column, HashMap, ScalarValue};
use datafusion_expr::Operator;
use std::collections::HashSet;
use std::fmt::{self, Display, Formatter};
use std::sync::Arc;

/// Represents a guarantee that must be true for a boolean expression to
/// evaluate to `true`.
///
/// The guarantee takes the form of a column and a set of literal (constant)
/// [`ScalarValue`]s. For the expression to evaluate to `true`, the column *must
/// satisfy* the guarantee(s).
///
/// To satisfy the guarantee, depending on [`Guarantee`], the values in the
/// column must either:
///
/// 1. be ONLY one of that set
/// 2. NOT be ANY of that set
///
/// # Uses `LiteralGuarantee`s
///
/// `LiteralGuarantee`s can be used to simplify filter expressions and skip data
/// files (e.g. row groups in parquet files) by proving expressions can not
/// possibly evaluate to `true`. For example, if we have a guarantee that `a`
/// must be in (`1`) for a filter to evaluate to `true`, then we can skip any
/// partition where we know that `a` never has the value of `1`.
///
/// **Important**: If a `LiteralGuarantee` is not satisfied, the relevant
/// expression is *guaranteed* to evaluate to `false` or `null`. **However**,
/// the opposite does not hold. Even if all `LiteralGuarantee`s are satisfied,
/// that does **not** guarantee that the predicate will actually evaluate to
/// `true`: it may still evaluate to `true`, `false` or `null`.
///
/// # Creating `LiteralGuarantee`s
///
/// Use [`LiteralGuarantee::analyze`] to extract literal guarantees from a
/// filter predicate.
///
/// # Details
/// A guarantee can be one of two forms:
///
/// 1. The column must be one the values for the predicate to be `true`. If the
///    column takes on any other value, the predicate can not evaluate to `true`.
///    For example,
///    `(a = 1)`, `(a = 1 OR a = 2)` or `a IN (1, 2, 3)`
///
/// 2. The column must NOT be one of the values for the predicate to be `true`.
///    If the column can ONLY take one of these values, the predicate can not
///    evaluate to `true`. For example,
///    `(a != 1)`, `(a != 1 AND a != 2)` or `a NOT IN (1, 2, 3)`
#[derive(Debug, Clone, PartialEq)]
pub struct LiteralGuarantee {
    pub column: Column,
    pub guarantee: Guarantee,
    pub literals: HashSet<ScalarValue>,
}

/// What is guaranteed about the values for a [`LiteralGuarantee`]?
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Guarantee {
    /// Guarantee that the expression is `true` if `column` is one of the values. If
    /// `column` is not one of the values, the expression can not be `true`.
    In,
    /// Guarantee that the expression is `true` if `column` is not ANY of the
    /// values. If `column` only takes one of these values, the expression can
    /// not be `true`.
    NotIn,
}

impl LiteralGuarantee {
    /// Create a new instance of the guarantee if the provided operator is
    /// supported. Returns None otherwise. See [`LiteralGuarantee::analyze`] to
    /// create these structures from an predicate (boolean expression).
    fn new<'a>(
        column_name: impl Into<String>,
        guarantee: Guarantee,
        literals: impl IntoIterator<Item = &'a ScalarValue>,
    ) -> Self {
        let literals: HashSet<_> = literals.into_iter().cloned().collect();

        Self {
            column: Column::from_name(column_name),
            guarantee,
            literals,
        }
    }

    /// Return a list of [`LiteralGuarantee`]s that must be satisfied for `expr`
    /// to evaluate to `true`.
    ///
    /// If more than one `LiteralGuarantee` is returned, they must **all** hold
    /// for the expression to possibly be `true`. If any is not satisfied, the
    /// expression is guaranteed to be `null` or `false`.
    ///
    /// # Notes:
    /// 1. `expr` must be a boolean expression or inlist expression.
    /// 2. `expr` is not simplified prior to analysis.
    pub fn analyze(expr: &Arc<dyn PhysicalExpr>) -> Vec<LiteralGuarantee> {
        // split conjunction: <expr> AND <expr> AND ...
        split_conjunction(expr)
            .into_iter()
            // for an `AND` conjunction to be true, all terms individually must be true
            .fold(GuaranteeBuilder::new(), |builder, expr| {
                if let Some(cel) = ColOpLit::try_new(expr) {
                    builder.aggregate_conjunct(cel)
                } else if let Some(inlist) = expr
                    .as_any()
                    .downcast_ref::<crate::expressions::InListExpr>()
                {
                    if let Some(inlist) = ColInList::try_new(inlist) {
                        builder.aggregate_multi_conjunct(
                            inlist.col,
                            inlist.guarantee,
                            inlist.list.iter().map(|lit| lit.value()),
                        )
                    } else {
                        builder
                    }
                } else {
                    // split disjunction: <expr> OR <expr> OR ...
                    let disjunctions = split_disjunction(expr);

                    // We are trying to add a guarantee that a column must be
                    // in/not in a particular set of values for the expression
                    // to evaluate to true.
                    //
                    // A disjunction is true, if at least one of the terms is be
                    // true.
                    //
                    // Thus, we can infer a guarantee if all terms are of the
                    // form `(col <op> literal) OR (col <op> literal) OR ...`.
                    //
                    // For example, we can infer that `a = 1 OR a = 2 OR a = 3`
                    // is guaranteed to be true ONLY if a is in (`1`, `2` or `3`).
                    //
                    // However, for something like  `a = 1 OR a = 2 OR a < 0` we
                    // **can't** guarantee that the predicate is only true if a
                    // is in (`1`, `2`), as it could also be true if `a` were less
                    // than zero.
                    let terms = disjunctions
                        .iter()
                        .filter_map(|expr| ColOpLit::try_new(expr))
                        .collect::<Vec<_>>();

                    // if all terms are 'col <op> literal' with the same column
                    // and operation we can infer any guarantees
                    //
                    // For those like (a != foo AND (a != bar OR a != baz)).
                    // We can't combine the (a != bar OR a != baz) part, but
                    // it also doesn't invalidate our knowledge that a !=
                    // foo is required for the expression to be true.
                    // So we can only create a multi value guarantee for `=`
                    // (or a single value). (e.g. ignore `a != foo OR a != bar`)
                    let first_term = terms.first();
                    if !terms.is_empty()
                        && terms.len() == disjunctions.len()
                        && terms.iter().all(|term| {
                            term.col.name() == first_term.unwrap().col.name()
                                && term.guarantee == Guarantee::In
                        })
                    {
                        builder.aggregate_multi_conjunct(
                            first_term.unwrap().col,
                            Guarantee::In,
                            terms.iter().map(|term| term.lit.value()),
                        )
                    } else {
                        // Handle disjunctions with conjunctions like (a = 1 AND b = 2) OR (a = 2 AND b = 3)
                        // Extract termsets from each disjunction
                        // if in each termset, they have same column, and the guarantee is In,
                        // we can infer a guarantee for the column
                        // e.g. (a = 1 AND b = 2) OR (a = 2 AND b = 3) is `a IN (1, 2) AND b IN (2, 3)`
                        // otherwise, we can't infer a guarantee
                        let termsets: Vec<Vec<ColOpLitOrInList>> = disjunctions
                            .iter()
                            .map(|expr| {
                                split_conjunction(expr)
                                    .into_iter()
                                    .filter_map(ColOpLitOrInList::try_new)
                                    .filter(|term| term.guarantee() == Guarantee::In)
                                    .collect()
                            })
                            .collect();

                        // Early return if any termset is empty (can't infer guarantees)
                        if termsets.iter().any(|terms| terms.is_empty()) {
                            return builder;
                        }

                        // Find columns that appear in all termsets
                        let common_cols = find_common_columns(&termsets);
                        if common_cols.is_empty() {
                            return builder;
                        }

                        // Build guarantees for common columns
                        let mut builder = builder;
                        for col in common_cols {
                            let literals: Vec<_> = termsets
                                .iter()
                                .filter_map(|terms| {
                                    terms.iter().find(|term| term.col() == col).map(
                                        |term| {
                                            term.lits().into_iter().map(|lit| lit.value())
                                        },
                                    )
                                })
                                .flatten()
                                .collect();

                            builder = builder.aggregate_multi_conjunct(
                                col,
                                Guarantee::In,
                                literals.into_iter(),
                            );
                        }

                        builder
                    }
                }
            })
            .build()
    }
}

impl Display for LiteralGuarantee {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let mut sorted_literals: Vec<_> =
            self.literals.iter().map(|lit| lit.to_string()).collect();
        sorted_literals.sort();
        match self.guarantee {
            Guarantee::In => write!(
                f,
                "{} in ({})",
                self.column.name,
                sorted_literals.join(", ")
            ),
            Guarantee::NotIn => write!(
                f,
                "{} not in ({})",
                self.column.name,
                sorted_literals.join(", ")
            ),
        }
    }
}

/// Combines conjuncts (aka terms `AND`ed together) into [`LiteralGuarantee`]s,
/// preserving insert order
#[derive(Debug, Default)]
struct GuaranteeBuilder<'a> {
    /// List of guarantees that have been created so far
    /// if we have determined a subsequent conjunct invalidates a guarantee
    /// e.g. `a = foo AND a = bar` then the relevant guarantee will be None
    guarantees: Vec<Option<LiteralGuarantee>>,

    /// Key is the (column name, guarantee type)
    /// Value is the index into `guarantees`
    map: HashMap<(&'a crate::expressions::Column, Guarantee), usize>,
}

impl<'a> GuaranteeBuilder<'a> {
    fn new() -> Self {
        Default::default()
    }

    /// Aggregate a new single `AND col <op> literal` term to this builder
    /// combining with existing guarantees if possible.
    ///
    /// # Examples
    /// * `AND (a = 1)`: `a` is guaranteed to be 1
    /// * `AND (a != 1)`: a is guaranteed to not be 1
    fn aggregate_conjunct(self, col_op_lit: ColOpLit<'a>) -> Self {
        self.aggregate_multi_conjunct(
            col_op_lit.col,
            col_op_lit.guarantee,
            [col_op_lit.lit.value()],
        )
    }

    /// Aggregates a new single column, multi literal term to this builder
    /// combining with previously known guarantees if possible.
    ///
    /// # Examples
    /// For the following examples, we can guarantee the expression is `true` if:
    /// * `AND (a = 1 OR a = 2 OR a = 3)`: a is in (1, 2, or 3)
    /// * `AND (a IN (1,2,3))`: a is in (1, 2, or 3)
    /// * `AND (a != 1 OR a != 2 OR a != 3)`: a is not in (1, 2, or 3)
    /// * `AND (a NOT IN (1,2,3))`: a is not in (1, 2, or 3)
    fn aggregate_multi_conjunct(
        mut self,
        col: &'a crate::expressions::Column,
        guarantee: Guarantee,
        new_values: impl IntoIterator<Item = &'a ScalarValue>,
    ) -> Self {
        let key = (col, guarantee);
        if let Some(index) = self.map.get(&key) {
            // already have a guarantee for this column
            let entry = &mut self.guarantees[*index];

            let Some(existing) = entry else {
                // determined the previous guarantee for this column has been
                // invalidated, nothing to do
                return self;
            };

            // Combine conjuncts if we have `a != foo AND a != bar`. `a = foo
            // AND a = bar` doesn't make logical sense so we don't optimize this
            // case
            match existing.guarantee {
                // knew that the column could not be a set of values
                //
                // For example, if we previously had `a != 5` and now we see
                // another `AND a != 6` we know that a must not be either 5 or 6
                // for the expression to be true
                Guarantee::NotIn => {
                    let new_values: HashSet<_> = new_values.into_iter().collect();
                    existing.literals.extend(new_values.into_iter().cloned());
                }
                Guarantee::In => {
                    let intersection = new_values
                        .into_iter()
                        .filter(|new_value| existing.literals.contains(*new_value))
                        .collect::<Vec<_>>();
                    // for an In guarantee, if the intersection is not empty,  we can extend the guarantee
                    // e.g. `a IN (1,2,3) AND a IN (2,3,4)` is `a IN (2,3)`
                    // otherwise, we invalidate the guarantee
                    // e.g. `a IN (1,2,3) AND a IN (4,5,6)` is `a IN ()`, which is invalid
                    if !intersection.is_empty() {
                        existing.literals = intersection.into_iter().cloned().collect();
                    } else {
                        // at least one was not, so invalidate the guarantee
                        *entry = None;
                    }
                }
            }
        } else {
            // This is a new guarantee
            let new_values: HashSet<_> = new_values.into_iter().collect();

            let guarantee = LiteralGuarantee::new(col.name(), guarantee, new_values);
            // add it to the list of guarantees
            self.guarantees.push(Some(guarantee));
            self.map.insert(key, self.guarantees.len() - 1);
        }

        self
    }

    /// Return all guarantees that have been created so far
    fn build(self) -> Vec<LiteralGuarantee> {
        // filter out any guarantees that have been invalidated
        self.guarantees.into_iter().flatten().collect()
    }
}

/// Represents a single `col [not]in literal` expression
struct ColOpLit<'a> {
    col: &'a crate::expressions::Column,
    guarantee: Guarantee,
    lit: &'a crate::expressions::Literal,
}

impl<'a> ColOpLit<'a> {
    /// Returns Some(ColOpLit) if the expression is either:
    /// 1. `col <op> literal`
    /// 2. `literal <op> col`
    /// 3. operator is `=` or `!=`
    ///
    /// Returns None otherwise
    fn try_new(expr: &'a Arc<dyn PhysicalExpr>) -> Option<Self> {
        let binary_expr = expr
            .as_any()
            .downcast_ref::<crate::expressions::BinaryExpr>()?;

        let (left, op, right) = (
            binary_expr.left().as_any(),
            binary_expr.op(),
            binary_expr.right().as_any(),
        );
        let guarantee = match op {
            Operator::Eq => Guarantee::In,
            Operator::NotEq => Guarantee::NotIn,
            _ => return None,
        };
        // col <op> literal
        if let (Some(col), Some(lit)) = (
            left.downcast_ref::<crate::expressions::Column>(),
            right.downcast_ref::<crate::expressions::Literal>(),
        ) {
            Some(Self {
                col,
                guarantee,
                lit,
            })
        }
        // literal <op> col
        else if let (Some(lit), Some(col)) = (
            left.downcast_ref::<crate::expressions::Literal>(),
            right.downcast_ref::<crate::expressions::Column>(),
        ) {
            Some(Self {
                col,
                guarantee,
                lit,
            })
        } else {
            None
        }
    }
}

/// Represents a single `col [not]in literal` expression
struct ColInList<'a> {
    col: &'a crate::expressions::Column,
    guarantee: Guarantee,
    list: Vec<&'a crate::expressions::Literal>,
}

impl<'a> ColInList<'a> {
    /// Returns Some(ColInList) if the expression is either:
    /// 1. `col <op> (literal1, literal2, ...)`
    /// 3. operator is `in` or `not in`
    ///
    /// Returns None otherwise
    fn try_new(inlist: &'a crate::expressions::InListExpr) -> Option<Self> {
        // Only support single-column inlist currently, multi-column inlist is not supported
        let col = inlist
            .expr()
            .as_any()
            .downcast_ref::<crate::expressions::Column>()?;

        let literals = inlist
            .list()
            .iter()
            .map(|e| e.as_any().downcast_ref::<crate::expressions::Literal>())
            .collect::<Option<Vec<_>>>()?;

        let guarantee = if inlist.negated() {
            Guarantee::NotIn
        } else {
            Guarantee::In
        };

        Some(Self {
            col,
            guarantee,
            list: literals,
        })
    }
}

/// Represents a single `col [not]in literal` expression or a single `col <op> literal` expression
enum ColOpLitOrInList<'a> {
    ColOpLit(ColOpLit<'a>),
    ColInList(ColInList<'a>),
}

impl<'a> ColOpLitOrInList<'a> {
    fn try_new(expr: &'a Arc<dyn PhysicalExpr>) -> Option<Self> {
        match expr
            .as_any()
            .downcast_ref::<crate::expressions::InListExpr>()
        {
            Some(inlist) => Some(Self::ColInList(ColInList::try_new(inlist)?)),
            None => ColOpLit::try_new(expr).map(Self::ColOpLit),
        }
    }

    fn guarantee(&self) -> Guarantee {
        match self {
            Self::ColOpLit(col_op_lit) => col_op_lit.guarantee,
            Self::ColInList(col_in_list) => col_in_list.guarantee,
        }
    }

    fn col(&self) -> &'a crate::expressions::Column {
        match self {
            Self::ColOpLit(col_op_lit) => col_op_lit.col,
            Self::ColInList(col_in_list) => col_in_list.col,
        }
    }

    fn lits(&self) -> Vec<&'a crate::expressions::Literal> {
        match self {
            Self::ColOpLit(col_op_lit) => vec![col_op_lit.lit],
            Self::ColInList(col_in_list) => col_in_list.list.clone(),
        }
    }
}

/// Find columns that appear in all termsets
fn find_common_columns<'a>(
    termsets: &[Vec<ColOpLitOrInList<'a>>],
) -> Vec<&'a crate::expressions::Column> {
    if termsets.is_empty() {
        return Vec::new();
    }

    // Start with columns from the first termset
    let mut common_cols: HashSet<_> = termsets[0].iter().map(|term| term.col()).collect();

    // check if any common_col in one termset occur many times
    // e.g. (a = 1 AND a = 2) OR (a = 2 AND b = 3), should not infer a guarantee
    // TODO: for above case, we can infer a IN (2) AND b IN (3)
    if common_cols.len() != termsets[0].len() {
        return Vec::new();
    }

    // Intersect with columns from remaining termsets
    for termset in termsets.iter().skip(1) {
        let termset_cols: HashSet<_> = termset.iter().map(|term| term.col()).collect();
        if termset_cols.len() != termset.len() {
            return Vec::new();
        }
        common_cols = common_cols.intersection(&termset_cols).cloned().collect();
    }

    common_cols.into_iter().collect()
}

#[cfg(test)]
mod test {
    use std::sync::LazyLock;

    use super::*;
    use crate::planner::logical2physical;

    use arrow::datatypes::{DataType, Field, Schema, SchemaRef};
    use datafusion_expr::expr_fn::*;
    use datafusion_expr::{lit, Expr};

    use itertools::Itertools;

    #[test]
    fn test_literal() {
        // a single literal offers no guarantee
        test_analyze(lit(true), vec![])
    }

    #[test]
    fn test_single() {
        // a = "foo"
        test_analyze(col("a").eq(lit("foo")), vec![in_guarantee("a", ["foo"])]);
        // "foo" = a
        test_analyze(lit("foo").eq(col("a")), vec![in_guarantee("a", ["foo"])]);
        // a != "foo"
        test_analyze(
            col("a").not_eq(lit("foo")),
            vec![not_in_guarantee("a", ["foo"])],
        );
        // "foo" != a
        test_analyze(
            lit("foo").not_eq(col("a")),
            vec![not_in_guarantee("a", ["foo"])],
        );
    }

    #[test]
    fn test_conjunction_single_column() {
        // b = 1 AND b = 2. This is impossible. Ideally this expression could be simplified to false
        test_analyze(col("b").eq(lit(1)).and(col("b").eq(lit(2))), vec![]);
        // b = 1 AND b != 2 . In theory, this could be simplified to `b = 1`.
        test_analyze(
            col("b").eq(lit(1)).and(col("b").not_eq(lit(2))),
            vec![
                // can only be true of b is 1 and b is not 2 (even though it is redundant)
                in_guarantee("b", [1]),
                not_in_guarantee("b", [2]),
            ],
        );
        // b != 1 AND b = 2. In theory, this could be simplified to `b = 2`.
        test_analyze(
            col("b").not_eq(lit(1)).and(col("b").eq(lit(2))),
            vec![
                // can only be true of b is not 1 and b is 2 (even though it is redundant)
                not_in_guarantee("b", [1]),
                in_guarantee("b", [2]),
            ],
        );
        // b != 1 AND b != 2
        test_analyze(
            col("b").not_eq(lit(1)).and(col("b").not_eq(lit(2))),
            vec![not_in_guarantee("b", [1, 2])],
        );
        // b != 1 AND b != 2 and b != 3
        test_analyze(
            col("b")
                .not_eq(lit(1))
                .and(col("b").not_eq(lit(2)))
                .and(col("b").not_eq(lit(3))),
            vec![not_in_guarantee("b", [1, 2, 3])],
        );
        // b != 1 AND b = 2 and b != 3. Can only be true if b is 2 and b is not in (1, 3)
        test_analyze(
            col("b")
                .not_eq(lit(1))
                .and(col("b").eq(lit(2)))
                .and(col("b").not_eq(lit(3))),
            vec![not_in_guarantee("b", [1, 3]), in_guarantee("b", [2])],
        );
        // b != 1 AND b != 2 and b = 3 (in theory could determine b = 3)
        test_analyze(
            col("b")
                .not_eq(lit(1))
                .and(col("b").not_eq(lit(2)))
                .and(col("b").eq(lit(3))),
            vec![not_in_guarantee("b", [1, 2]), in_guarantee("b", [3])],
        );
        // b != 1 AND b != 2 and b > 3 (to be true, b can't be either 1 or 2
        test_analyze(
            col("b")
                .not_eq(lit(1))
                .and(col("b").not_eq(lit(2)))
                .and(col("b").gt(lit(3))),
            vec![not_in_guarantee("b", [1, 2])],
        );
    }

    #[test]
    fn test_conjunction_multi_column() {
        // a = "foo" AND b = 1
        test_analyze(
            col("a").eq(lit("foo")).and(col("b").eq(lit(1))),
            vec![
                // should find both column guarantees
                in_guarantee("a", ["foo"]),
                in_guarantee("b", [1]),
            ],
        );
        // a != "foo" AND b != 1
        test_analyze(
            col("a").not_eq(lit("foo")).and(col("b").not_eq(lit(1))),
            // should find both column guarantees
            vec![not_in_guarantee("a", ["foo"]), not_in_guarantee("b", [1])],
        );
        // a = "foo" AND a = "bar"
        test_analyze(
            col("a").eq(lit("foo")).and(col("a").eq(lit("bar"))),
            // this predicate is impossible ( can't be both foo and bar),
            vec![],
        );
        // a = "foo" AND b != "bar"
        test_analyze(
            col("a").eq(lit("foo")).and(col("a").not_eq(lit("bar"))),
            vec![in_guarantee("a", ["foo"]), not_in_guarantee("a", ["bar"])],
        );
        // a != "foo" AND a != "bar"
        test_analyze(
            col("a").not_eq(lit("foo")).and(col("a").not_eq(lit("bar"))),
            // know it isn't "foo" or "bar"
            vec![not_in_guarantee("a", ["foo", "bar"])],
        );
        // a != "foo" AND a != "bar" and a != "baz"
        test_analyze(
            col("a")
                .not_eq(lit("foo"))
                .and(col("a").not_eq(lit("bar")))
                .and(col("a").not_eq(lit("baz"))),
            // know it isn't "foo" or "bar" or "baz"
            vec![not_in_guarantee("a", ["foo", "bar", "baz"])],
        );
        // a = "foo" AND a = "foo"
        let expr = col("a").eq(lit("foo"));
        test_analyze(expr.clone().and(expr), vec![in_guarantee("a", ["foo"])]);
        // b > 5 AND b = 10 (should get an b = 10 guarantee)
        test_analyze(
            col("b").gt(lit(5)).and(col("b").eq(lit(10))),
            vec![in_guarantee("b", [10])],
        );
        // b > 10 AND b = 10 (this is impossible)
        test_analyze(
            col("b").gt(lit(10)).and(col("b").eq(lit(10))),
            vec![
                //  if b isn't 10, it can not be true (though the expression actually can never be true)
                in_guarantee("b", [10]),
            ],
        );
        // a != "foo" and (a != "bar" OR a != "baz")
        test_analyze(
            col("a")
                .not_eq(lit("foo"))
                .and(col("a").not_eq(lit("bar")).or(col("a").not_eq(lit("baz")))),
            // a is not foo (we can't represent other knowledge about a)
            vec![not_in_guarantee("a", ["foo"])],
        );
    }

    #[test]
    fn test_conjunction_and_disjunction_single_column() {
        // b != 1 AND (b > 2)
        test_analyze(
            col("b").not_eq(lit(1)).and(col("b").gt(lit(2))),
            vec![
                // for the expression to be true, b can not be one
                not_in_guarantee("b", [1]),
            ],
        );

        // b = 1 AND (b = 2 OR b = 3). Could be simplified to false.
        test_analyze(
            col("b")
                .eq(lit(1))
                .and(col("b").eq(lit(2)).or(col("b").eq(lit(3)))),
            vec![
                // in theory, b must be 1 and one of 2,3 for this expression to be true
                // which is a logical contradiction
            ],
        );
    }

    #[test]
    fn test_disjunction_single_column() {
        // b = 1 OR b = 2
        test_analyze(
            col("b").eq(lit(1)).or(col("b").eq(lit(2))),
            vec![in_guarantee("b", [1, 2])],
        );
        // b != 1 OR b = 2
        test_analyze(col("b").not_eq(lit(1)).or(col("b").eq(lit(2))), vec![]);
        // b = 1 OR b != 2
        test_analyze(col("b").eq(lit(1)).or(col("b").not_eq(lit(2))), vec![]);
        // b != 1 OR b != 2
        test_analyze(col("b").not_eq(lit(1)).or(col("b").not_eq(lit(2))), vec![]);
        // b != 1 OR b != 2 OR b = 3 -- in theory could guarantee that b = 3
        test_analyze(
            col("b")
                .not_eq(lit(1))
                .or(col("b").not_eq(lit(2)))
                .or(lit("b").eq(lit(3))),
            vec![],
        );
        // b = 1 OR b = 2 OR b = 3
        test_analyze(
            col("b")
                .eq(lit(1))
                .or(col("b").eq(lit(2)))
                .or(col("b").eq(lit(3))),
            vec![in_guarantee("b", [1, 2, 3])],
        );
        // b = 1 OR b = 2 OR b > 3 -- can't guarantee that the expression is only true if a is in (1, 2)
        test_analyze(
            col("b")
                .eq(lit(1))
                .or(col("b").eq(lit(2)))
                .or(lit("b").eq(lit(3))),
            vec![],
        );
    }

    #[test]
    fn test_disjunction_multi_column() {
        // a = "foo" OR b = 1
        test_analyze(
            col("a").eq(lit("foo")).or(col("b").eq(lit(1))),
            // no can't have a single column guarantee (if a = "foo" then b != 1) etc
            vec![],
        );
        // a != "foo" OR b != 1
        test_analyze(
            col("a").not_eq(lit("foo")).or(col("b").not_eq(lit(1))),
            // No single column guarantee
            vec![],
        );
        // a = "foo" OR a = "bar"
        test_analyze(
            col("a").eq(lit("foo")).or(col("a").eq(lit("bar"))),
            vec![in_guarantee("a", ["foo", "bar"])],
        );
        // a = "foo" OR a = "foo"
        test_analyze(
            col("a").eq(lit("foo")).or(col("a").eq(lit("foo"))),
            vec![in_guarantee("a", ["foo"])],
        );
        // a != "foo" OR a != "bar"
        test_analyze(
            col("a").not_eq(lit("foo")).or(col("a").not_eq(lit("bar"))),
            // can't represent knowledge about a in this case
            vec![],
        );
        // a = "foo" OR a = "bar" OR a = "baz"
        test_analyze(
            col("a")
                .eq(lit("foo"))
                .or(col("a").eq(lit("bar")))
                .or(col("a").eq(lit("baz"))),
            vec![in_guarantee("a", ["foo", "bar", "baz"])],
        );
        // (a = "foo" OR a = "bar") AND (a = "baz)"
        test_analyze(
            (col("a").eq(lit("foo")).or(col("a").eq(lit("bar"))))
                .and(col("a").eq(lit("baz"))),
            // this could potentially be represented as 2 constraints with a more
            // sophisticated analysis
            vec![],
        );
        // (a = "foo" OR a = "bar") AND (b = 1)
        test_analyze(
            (col("a").eq(lit("foo")).or(col("a").eq(lit("bar"))))
                .and(col("b").eq(lit(1))),
            vec![in_guarantee("a", ["foo", "bar"]), in_guarantee("b", [1])],
        );
        // (a = "foo" OR a = "bar") OR (b = 1)
        test_analyze(
            col("a")
                .eq(lit("foo"))
                .or(col("a").eq(lit("bar")))
                .or(col("b").eq(lit(1))),
            // can't represent knowledge about a or b in this case
            vec![],
        );
    }

    #[test]
    fn test_single_inlist() {
        // b IN (1, 2, 3)
        test_analyze(
            col("b").in_list(vec![lit(1), lit(2), lit(3)], false),
            vec![in_guarantee("b", [1, 2, 3])],
        );
        // b NOT IN (1, 2, 3)
        test_analyze(
            col("b").in_list(vec![lit(1), lit(2), lit(3)], true),
            vec![not_in_guarantee("b", [1, 2, 3])],
        );
        // b IN (1,2,3,4...24)
        test_analyze(
            col("b").in_list((1..25).map(lit).collect_vec(), false),
            vec![in_guarantee("b", 1..25)],
        );
    }

    #[test]
    fn test_inlist_conjunction() {
        // b IN (1, 2, 3) AND b IN (2, 3, 4)
        test_analyze(
            col("b")
                .in_list(vec![lit(1), lit(2), lit(3)], false)
                .and(col("b").in_list(vec![lit(2), lit(3), lit(4)], false)),
            vec![in_guarantee("b", [2, 3])],
        );
        // b NOT IN (1, 2, 3) AND b IN (2, 3, 4)
        test_analyze(
            col("b")
                .in_list(vec![lit(1), lit(2), lit(3)], true)
                .and(col("b").in_list(vec![lit(2), lit(3), lit(4)], false)),
            vec![
                not_in_guarantee("b", [1, 2, 3]),
                in_guarantee("b", [2, 3, 4]),
            ],
        );
        // b NOT IN (1, 2, 3) AND b NOT IN (2, 3, 4)
        test_analyze(
            col("b")
                .in_list(vec![lit(1), lit(2), lit(3)], true)
                .and(col("b").in_list(vec![lit(2), lit(3), lit(4)], true)),
            vec![not_in_guarantee("b", [1, 2, 3, 4])],
        );
        // b IN (1, 2, 3) AND b = 4
        test_analyze(
            col("b")
                .in_list(vec![lit(1), lit(2), lit(3)], false)
                .and(col("b").eq(lit(4))),
            vec![],
        );
        // b IN (1, 2, 3) AND b = 2
        test_analyze(
            col("b")
                .in_list(vec![lit(1), lit(2), lit(3)], false)
                .and(col("b").eq(lit(2))),
            vec![in_guarantee("b", [2])],
        );
        // b IN (1, 2, 3) AND b != 2
        test_analyze(
            col("b")
                .in_list(vec![lit(1), lit(2), lit(3)], false)
                .and(col("b").not_eq(lit(2))),
            vec![in_guarantee("b", [1, 2, 3]), not_in_guarantee("b", [2])],
        );
        // b NOT IN (1, 2, 3) AND b != 4
        test_analyze(
            col("b")
                .in_list(vec![lit(1), lit(2), lit(3)], true)
                .and(col("b").not_eq(lit(4))),
            vec![not_in_guarantee("b", [1, 2, 3, 4])],
        );
        // b NOT IN (1, 2, 3) AND b != 2
        test_analyze(
            col("b")
                .in_list(vec![lit(1), lit(2), lit(3)], true)
                .and(col("b").not_eq(lit(2))),
            vec![not_in_guarantee("b", [1, 2, 3])],
        );
    }

    #[test]
    fn test_inlist_with_disjunction() {
        // b IN (1, 2, 3) AND (b = 3 OR b = 4)
        test_analyze(
            col("b")
                .in_list(vec![lit(1), lit(2), lit(3)], false)
                .and(col("b").eq(lit(3)).or(col("b").eq(lit(4)))),
            vec![in_guarantee("b", [3])],
        );
        // b IN (1, 2, 3) AND (b = 4 OR b = 5)
        test_analyze(
            col("b")
                .in_list(vec![lit(1), lit(2), lit(3)], false)
                .and(col("b").eq(lit(4)).or(col("b").eq(lit(5)))),
            vec![],
        );
        // b NOT IN (1, 2, 3) AND (b = 3 OR b = 4)
        test_analyze(
            col("b")
                .in_list(vec![lit(1), lit(2), lit(3)], true)
                .and(col("b").eq(lit(3)).or(col("b").eq(lit(4)))),
            vec![not_in_guarantee("b", [1, 2, 3]), in_guarantee("b", [3, 4])],
        );
        // b IN (1, 2, 3) OR b = 2
        test_analyze(
            col("b")
                .in_list(vec![lit(1), lit(2), lit(3)], false)
                .or(col("b").eq(lit(2))),
            vec![in_guarantee("b", [1, 2, 3])],
        );
        // b IN (1, 2, 3) OR b != 3
        test_analyze(
            col("b")
                .in_list(vec![lit(1), lit(2), lit(3)], false)
                .or(col("b").not_eq(lit(3))),
            vec![],
        );
    }

    #[test]
    fn test_disjunction_and_conjunction_multi_column() {
        // (a = "foo" AND b = 1) OR (a = "bar" AND b = 2)
        test_analyze(
            (col("a").eq(lit("foo")).and(col("b").eq(lit(1))))
                .or(col("a").eq(lit("bar")).and(col("b").eq(lit(2)))),
            vec![in_guarantee("a", ["foo", "bar"]), in_guarantee("b", [1, 2])],
        );
        // (a = "foo" AND b = 1) OR (a = "bar" AND b = 2) OR (b = 3)
        test_analyze(
            (col("a").eq(lit("foo")).and(col("b").eq(lit(1))))
                .or(col("a").eq(lit("bar")).and(col("b").eq(lit(2))))
                .or(col("b").eq(lit(3))),
            vec![in_guarantee("b", [1, 2, 3])],
        );
        // (a = "foo" AND b = 1) OR (a = "bar" AND b = 2) OR (c = 3)
        test_analyze(
            (col("a").eq(lit("foo")).and(col("b").eq(lit(1))))
                .or(col("a").eq(lit("bar")).and(col("b").eq(lit(2))))
                .or(col("c").eq(lit(3))),
            vec![],
        );
        // (a = "foo" AND b > 1) OR (a = "bar" AND b = 2)
        test_analyze(
            (col("a").eq(lit("foo")).and(col("b").gt(lit(1))))
                .or(col("a").eq(lit("bar")).and(col("b").eq(lit(2)))),
            vec![in_guarantee("a", ["foo", "bar"])],
        );
        // (a = "foo" AND b = 1) OR (b = 1 AND c = 2) OR (c = 3 AND a = "bar")
        test_analyze(
            (col("a").eq(lit("foo")).and(col("b").eq(lit(1))))
                .or(col("b").eq(lit(1)).and(col("c").eq(lit(2))))
                .or(col("c").eq(lit(3)).and(col("a").eq(lit("bar")))),
            vec![],
        );
        // (a = "foo" AND a = "bar") OR (a = "good" AND b = 1)
        // TODO: this should be `a IN ("good") AND b IN (1)`
        test_analyze(
            (col("a").eq(lit("foo")).and(col("a").eq(lit("bar"))))
                .or(col("a").eq(lit("good")).and(col("b").eq(lit(1)))),
            vec![],
        );
        // (a = "foo" AND a = "foo") OR (a = "good" AND b = 1)
        // TODO: this should be `a IN ("foo", "good")`
        test_analyze(
            (col("a").eq(lit("foo")).and(col("a").eq(lit("foo"))))
                .or(col("a").eq(lit("good")).and(col("b").eq(lit(1)))),
            vec![],
        );
        // (a = "foo" AND b = 3) OR (b = 4 AND b = 1) OR (b = 2 AND a = "bar")
        test_analyze(
            (col("a").eq(lit("foo")).and(col("b").eq(lit(3))))
                .or(col("b").eq(lit(4)).and(col("b").eq(lit(1))))
                .or(col("b").eq(lit(2)).and(col("a").eq(lit("bar")))),
            vec![],
        );
        // (b = 1 AND b > 3) OR (a = "foo" AND b = 4)
        test_analyze(
            (col("b").eq(lit(1)).and(col("b").gt(lit(3))))
                .or(col("a").eq(lit("foo")).and(col("b").eq(lit(4)))),
            // if b isn't 1 or 4, it can not be true (though the expression actually can never be true)
            vec![in_guarantee("b", [1, 4])],
        );
        // (a = "foo" AND b = 1) OR (a != "bar" AND b = 2)
        test_analyze(
            (col("a").eq(lit("foo")).and(col("b").eq(lit(1))))
                .or(col("a").not_eq(lit("bar")).and(col("b").eq(lit(2)))),
            vec![in_guarantee("b", [1, 2])],
        );
        // (a = "foo" AND b = 1) OR (a LIKE "%bar" AND b = 2)
        test_analyze(
            (col("a").eq(lit("foo")).and(col("b").eq(lit(1))))
                .or(col("a").like(lit("%bar")).and(col("b").eq(lit(2)))),
            vec![in_guarantee("b", [1, 2])],
        );
        // (a IN ("foo", "bar") AND b = 5) OR (a IN ("foo", "bar") AND b = 6)
        test_analyze(
            (col("a")
                .in_list(vec![lit("foo"), lit("bar")], false)
                .and(col("b").eq(lit(5))))
            .or(col("a")
                .in_list(vec![lit("foo"), lit("bar")], false)
                .and(col("b").eq(lit(6)))),
            vec![in_guarantee("a", ["foo", "bar"]), in_guarantee("b", [5, 6])],
        );
        // (a IN ("foo", "bar") AND b = 5) OR (a IN ("foo") AND b = 6)
        test_analyze(
            (col("a")
                .in_list(vec![lit("foo"), lit("bar")], false)
                .and(col("b").eq(lit(5))))
            .or(col("a")
                .in_list(vec![lit("foo")], false)
                .and(col("b").eq(lit(6)))),
            vec![in_guarantee("a", ["foo", "bar"]), in_guarantee("b", [5, 6])],
        );
        // (a NOT IN ("foo", "bar") AND b = 5) OR (a NOT IN ("foo") AND b = 6)
        test_analyze(
            (col("a")
                .in_list(vec![lit("foo"), lit("bar")], true)
                .and(col("b").eq(lit(5))))
            .or(col("a")
                .in_list(vec![lit("foo")], true)
                .and(col("b").eq(lit(6)))),
            vec![in_guarantee("b", [5, 6])],
        );
    }

    /// Tests that analyzing expr results in the expected guarantees
    fn test_analyze(expr: Expr, expected: Vec<LiteralGuarantee>) {
        println!("Begin analyze of {expr}");
        let schema = schema();
        let physical_expr = logical2physical(&expr, &schema);

        let actual = LiteralGuarantee::analyze(&physical_expr)
            .into_iter()
            .sorted_by_key(|g| g.column.name().to_string())
            .collect::<Vec<_>>();
        assert_eq!(
            expected, actual,
            "expr: {expr}\
               \n\nexpected: {expected:#?}\
               \n\nactual: {actual:#?}\
               \n\nexpr: {expr:#?}\
               \n\nphysical_expr: {physical_expr:#?}"
        );
    }

    /// Guarantee that the expression is true if the column is one of the specified values
    fn in_guarantee<'a, I, S>(column: &str, literals: I) -> LiteralGuarantee
    where
        I: IntoIterator<Item = S>,
        S: Into<ScalarValue> + 'a,
    {
        let literals: Vec<_> = literals.into_iter().map(|s| s.into()).collect();
        LiteralGuarantee::new(column, Guarantee::In, literals.iter())
    }

    /// Guarantee that the expression is true if the column is NOT any of the specified values
    fn not_in_guarantee<'a, I, S>(column: &str, literals: I) -> LiteralGuarantee
    where
        I: IntoIterator<Item = S>,
        S: Into<ScalarValue> + 'a,
    {
        let literals: Vec<_> = literals.into_iter().map(|s| s.into()).collect();
        LiteralGuarantee::new(column, Guarantee::NotIn, literals.iter())
    }

    // Schema for testing
    fn schema() -> SchemaRef {
        static SCHEMA: LazyLock<SchemaRef> = LazyLock::new(|| {
            Arc::new(Schema::new(vec![
                Field::new("a", DataType::Utf8, false),
                Field::new("b", DataType::Int32, false),
                Field::new("c", DataType::Int32, false),
            ]))
        });
        Arc::clone(&SCHEMA)
    }
}
