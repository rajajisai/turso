use turso_sqlite3_parser::ast::{self, SortOrder};

use crate::{
    schema::PseudoCursorType,
    translate::collate::CollationSeq,
    util::exprs_are_equivalent,
    vdbe::{
        builder::{CursorType, ProgramBuilder},
        insn::Insn,
    },
    Result,
};

use super::{
    emitter::{Resolver, TranslateCtx},
    expr::translate_expr,
    plan::{Distinctness, ResultSetColumn, SelectPlan, TableReferences},
    result_row::{emit_offset, emit_result_row_and_limit},
};

// Metadata for handling ORDER BY operations
#[derive(Debug)]
pub struct SortMetadata {
    // cursor id for the Sorter table where the sorted rows are stored
    pub sort_cursor: usize,
    // register where the sorter data is inserted and later retrieved from
    pub reg_sorter_data: usize,
}

/// Initialize resources needed for ORDER BY processing
pub fn init_order_by(
    program: &mut ProgramBuilder,
    t_ctx: &mut TranslateCtx,
    order_by: &[(ast::Expr, SortOrder)],
    referenced_tables: &TableReferences,
) -> Result<()> {
    let sort_cursor = program.alloc_cursor_id(CursorType::Sorter);
    t_ctx.meta_sort = Some(SortMetadata {
        sort_cursor,
        reg_sorter_data: program.alloc_register(),
    });

    /*
     * Terms of the ORDER BY clause that is part of a SELECT statement may be assigned a collating sequence using the COLLATE operator,
     * in which case the specified collating function is used for sorting.
     * Otherwise, if the expression sorted by an ORDER BY clause is a column,
     * then the collating sequence of the column is used to determine sort order.
     * If the expression is not a column and has no COLLATE clause, then the BINARY collating sequence is used.
     */
    let collations = order_by
        .iter()
        .map(|(expr, _)| match expr {
            ast::Expr::Collate(_, collation_name) => CollationSeq::new(collation_name).map(Some),
            ast::Expr::Column { table, column, .. } => {
                let table = referenced_tables.find_table_by_internal_id(*table).unwrap();

                let Some(table_column) = table.get_column_at(*column) else {
                    crate::bail_parse_error!("column index out of bounds");
                };

                Ok(table_column.collation)
            }
            _ => Ok(Some(CollationSeq::default())),
        })
        .collect::<Result<Vec<_>>>()?;
    program.emit_insn(Insn::SorterOpen {
        cursor_id: sort_cursor,
        columns: order_by.len(),
        order: order_by.iter().map(|(_, direction)| *direction).collect(),
        collations,
    });
    Ok(())
}

/// Emits the bytecode for outputting rows from an ORDER BY sorter.
/// This is called when the main query execution loop has finished processing,
/// and we can now emit rows from the ORDER BY sorter.
pub fn emit_order_by(
    program: &mut ProgramBuilder,
    t_ctx: &mut TranslateCtx,
    plan: &SelectPlan,
) -> Result<()> {
    let order_by = plan.order_by.as_ref().unwrap();
    let result_columns = &plan.result_columns;
    let sort_loop_start_label = program.allocate_label();
    let sort_loop_next_label = program.allocate_label();
    let sort_loop_end_label = program.allocate_label();

    let sorter_column_count = order_by.len() + result_columns.len()
        - t_ctx
            .result_columns_to_skip_in_orderby_sorter
            .as_ref()
            .map(|v| v.len())
            .unwrap_or(0);

    let pseudo_cursor = program.alloc_cursor_id(CursorType::Pseudo(PseudoCursorType {
        column_count: sorter_column_count,
    }));
    let SortMetadata {
        sort_cursor,
        reg_sorter_data,
    } = *t_ctx.meta_sort.as_mut().unwrap();

    program.emit_insn(Insn::OpenPseudo {
        cursor_id: pseudo_cursor,
        content_reg: reg_sorter_data,
        num_fields: sorter_column_count,
    });

    program.emit_insn(Insn::SorterSort {
        cursor_id: sort_cursor,
        pc_if_empty: sort_loop_end_label,
    });
    program.preassign_label_to_next_insn(sort_loop_start_label);

    emit_offset(program, plan, sort_loop_next_label, t_ctx.reg_offset);

    program.emit_insn(Insn::SorterData {
        cursor_id: sort_cursor,
        dest_reg: reg_sorter_data,
        pseudo_cursor,
    });

    // We emit the columns in SELECT order, not sorter order (sorter always has the sort keys first).
    // This is tracked in m.result_column_indexes_in_orderby_sorter.
    let cursor_id = pseudo_cursor;
    let start_reg = t_ctx.reg_result_cols_start.unwrap();
    for i in 0..result_columns.len() {
        let reg = start_reg + i;
        program.emit_column(
            cursor_id,
            t_ctx.result_column_indexes_in_orderby_sorter[i],
            reg,
        );
    }

    emit_result_row_and_limit(
        program,
        plan,
        start_reg,
        t_ctx.limit_ctx,
        Some(sort_loop_end_label),
    )?;

    program.resolve_label(sort_loop_next_label, program.offset());
    program.emit_insn(Insn::SorterNext {
        cursor_id: sort_cursor,
        pc_if_next: sort_loop_start_label,
    });
    program.preassign_label_to_next_insn(sort_loop_end_label);

    Ok(())
}

/// Emits the bytecode for inserting a row into an ORDER BY sorter.
pub fn order_by_sorter_insert(
    program: &mut ProgramBuilder,
    resolver: &Resolver,
    sort_metadata: &SortMetadata,
    res_col_indexes_in_orderby_sorter: &mut Vec<usize>,
    plan: &SelectPlan,
) -> Result<()> {
    let order_by = plan.order_by.as_ref().unwrap();
    let order_by_len = order_by.len();
    let result_columns = &plan.result_columns;
    // If any result columns can be skipped due to being an exact duplicate of a sort key, we need to know which ones and their new index in the ORDER BY sorter.
    let result_columns_to_skip = order_by_deduplicate_result_columns(order_by, result_columns);
    let result_columns_to_skip_len = result_columns_to_skip
        .as_ref()
        .map(|v| v.len())
        .unwrap_or(0);

    // The ORDER BY sorter has the sort keys first, then the result columns.
    let orderby_sorter_column_count =
        order_by_len + result_columns.len() - result_columns_to_skip_len;
    let start_reg = program.alloc_registers(orderby_sorter_column_count);
    for (i, (expr, _)) in order_by.iter().enumerate() {
        let key_reg = start_reg + i;
        translate_expr(
            program,
            Some(&plan.table_references),
            expr,
            key_reg,
            resolver,
        )?;
    }
    let mut cur_reg = start_reg + order_by_len;
    let mut cur_idx_in_orderby_sorter = order_by_len;
    let mut translated_result_col_count = 0;
    for (i, rc) in result_columns.iter().enumerate() {
        if let Some(ref v) = result_columns_to_skip {
            let found = v.iter().find(|(skipped_idx, _)| *skipped_idx == i);
            // If the result column is in the list of columns to skip, we need to know its new index in the ORDER BY sorter.
            if let Some((_, result_column_idx)) = found {
                res_col_indexes_in_orderby_sorter.insert(i, *result_column_idx);
                continue;
            }
        }
        translate_expr(
            program,
            Some(&plan.table_references),
            &rc.expr,
            cur_reg,
            resolver,
        )?;
        translated_result_col_count += 1;
        res_col_indexes_in_orderby_sorter.insert(i, cur_idx_in_orderby_sorter);
        cur_idx_in_orderby_sorter += 1;
        cur_reg += 1;
    }

    // Handle SELECT DISTINCT deduplication
    if let Distinctness::Distinct { ctx } = &plan.distinctness {
        let distinct_ctx = ctx.as_ref().expect("distinct context must exist");
        let num_regs = order_by_len + translated_result_col_count;
        distinct_ctx.emit_deduplication_insns(program, num_regs, start_reg);
    }

    let SortMetadata {
        sort_cursor,
        reg_sorter_data,
    } = sort_metadata;

    sorter_insert(
        program,
        start_reg,
        orderby_sorter_column_count,
        *sort_cursor,
        *reg_sorter_data,
    );
    Ok(())
}

/// Emits the bytecode for inserting a row into a sorter.
/// This can be either a GROUP BY sorter or an ORDER BY sorter.
pub fn sorter_insert(
    program: &mut ProgramBuilder,
    start_reg: usize,
    column_count: usize,
    cursor_id: usize,
    record_reg: usize,
) {
    program.emit_insn(Insn::MakeRecord {
        start_reg,
        count: column_count,
        dest_reg: record_reg,
        index_name: None,
    });
    program.emit_insn(Insn::SorterInsert {
        cursor_id,
        record_reg,
    });
}

/// In case any of the ORDER BY sort keys are exactly equal to a result column, we can skip emitting that result column.
/// If we skip a result column, we need to keep track what index in the ORDER BY sorter the result columns have,
/// because the result columns should be emitted in the SELECT clause order, not the ORDER BY clause order.
///
/// If any result columns can be skipped, this returns list of 2-tuples of (SkippedResultColumnIndex: usize, ResultColumnIndexInOrderBySorter: usize)
pub fn order_by_deduplicate_result_columns(
    order_by: &[(ast::Expr, SortOrder)],
    result_columns: &[ResultSetColumn],
) -> Option<Vec<(usize, usize)>> {
    let mut result_column_remapping: Option<Vec<(usize, usize)>> = None;
    for (i, rc) in result_columns.iter().enumerate() {
        let found = order_by
            .iter()
            .enumerate()
            .find(|(_, (expr, _))| exprs_are_equivalent(expr, &rc.expr));
        if let Some((j, _)) = found {
            if let Some(ref mut v) = result_column_remapping {
                v.push((i, j));
            } else {
                result_column_remapping = Some(vec![(i, j)]);
            }
        }
    }

    result_column_remapping
}
