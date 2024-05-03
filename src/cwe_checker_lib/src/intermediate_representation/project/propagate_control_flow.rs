use crate::analysis::graph::{self, Edge, Graph, Node};
use crate::intermediate_representation::*;

use std::collections::{BTreeSet, HashMap, HashSet};

use petgraph::graph::NodeIndex;
use petgraph::Direction::Incoming;

/// The `propagate_control_flow` normalization pass tries to simplify the representation of
/// sequences of if-else blocks that all have the same condition
/// so that they are either all executed or none of the blocks are executed.
/// Such sequences are often generated by sequences of conditional assignment assembly instructions.
///
/// To simplify the generated control flow graph
/// (and thus propagate the knowledge that either all or none of these blocks are executed to the control flow graph)
/// we look for sequences of (conditional) jumps where the final jump target is determined by the source of the first jump
/// (because we know that the conditionals for all jumps evaluate to the same value along the sequence).
/// For such a sequence we then retarget the destination of the first jump to the final jump destination of the sequence.
/// Lastly, the newly bypassed blocks are considered dead code and are removed.
pub fn propagate_control_flow(project: &mut Project) {
    let cfg_before_normalization = graph::get_program_cfg(&project.program);
    let nodes_without_incoming_edges_at_beginning =
        get_nodes_without_incoming_edge(&cfg_before_normalization);

    let mut jmps_to_retarget = HashMap::new();
    for node in cfg_before_normalization.node_indices() {
        let Node::BlkStart(block, sub) = cfg_before_normalization[node] else {
            continue;
        };
        // Conditions that we know to be true "on" a particular outgoing
        // edge.
        let mut true_conditions = Vec::new();
        if let Some(block_precondition) =
            get_block_precondition_after_defs(&cfg_before_normalization, node)
        {
            true_conditions.push(block_precondition);
        }
        match &block.term.jmps[..] {
            [Term {
                tid: call_tid,
                term:
                    Jmp::Call {
                        target: _,
                        return_: Some(return_target),
                    },
            }]
            | [Term {
                tid: call_tid,
                term:
                    Jmp::CallInd {
                        target: _,
                        return_: Some(return_target),
                    },
            }]
            | [Term {
                tid: call_tid,
                term:
                    Jmp::CallOther {
                        description: _,
                        return_: Some(return_target),
                    },
            }] => {
                if let Some(new_target) = find_target_for_retargetable_jump(
                    return_target,
                    &sub.term,
                    // Call may have side-effects that invalidate our
                    // knowledge about any condition we know to be true
                    // after execution of all DEFs in a block.
                    &Vec::new(),
                ) {
                    jmps_to_retarget.insert(call_tid.clone(), new_target);
                }
            }
            [Term {
                tid: jump_tid,
                term: Jmp::Branch(target),
            }] => {
                if let Some(new_target) =
                    find_target_for_retargetable_jump(target, &sub.term, &true_conditions)
                {
                    jmps_to_retarget.insert(jump_tid.clone(), new_target);
                }
            }
            [Term {
                term:
                    Jmp::CBranch {
                        condition,
                        target: if_target,
                    },
                tid: jump_tid_if,
            }, Term {
                term: Jmp::Branch(else_target),
                tid: jump_tid_else,
            }] => {
                true_conditions.push(condition.clone());
                if let Some(new_target) =
                    find_target_for_retargetable_jump(if_target, &sub.term, &true_conditions)
                {
                    jmps_to_retarget.insert(jump_tid_if.clone(), new_target);
                }

                let condition = true_conditions.pop().unwrap();
                true_conditions.push(negate_condition(condition));
                if let Some(new_target) =
                    find_target_for_retargetable_jump(else_target, &sub.term, &true_conditions)
                {
                    jmps_to_retarget.insert(jump_tid_else.clone(), new_target);
                }
            }
            _ => (),
        }
    }
    retarget_jumps(project, jmps_to_retarget);

    let cfg_after_normalization = graph::get_program_cfg(&project.program);
    let nodes_without_incoming_edges_at_end =
        get_nodes_without_incoming_edge(&cfg_after_normalization);

    remove_new_orphaned_blocks(
        project,
        nodes_without_incoming_edges_at_beginning,
        nodes_without_incoming_edges_at_end,
    );
}

/// Insert the new target TIDs into jump instructions for which a new target was computed.
fn retarget_jumps(project: &mut Project, mut jmps_to_retarget: HashMap<Tid, Tid>) {
    for sub in project.program.term.subs.values_mut() {
        for blk in sub.term.blocks.iter_mut() {
            for jmp in blk.term.jmps.iter_mut() {
                let Some(new_target) = jmps_to_retarget.remove(&jmp.tid) else {
                    continue;
                };
                match &mut jmp.term {
                    Jmp::Branch(target)
                    | Jmp::CBranch { target, .. }
                    | Jmp::Call {
                        target: _,
                        return_: Some(target),
                    }
                    | Jmp::CallInd {
                        target: _,
                        return_: Some(target),
                    }
                    | Jmp::CallOther {
                        description: _,
                        return_: Some(target),
                    } => *target = new_target,
                    _ => panic!("Unexpected type of jump encountered."),
                }
            }
        }
    }
}

/// Under the assumption that the given `true_condition` expression evaluates to `true`,
/// check whether we can retarget jumps to the given target to another final jump target.
/// I.e. we follow sequences of jumps that are not interrupted by [`Def`] instructions to their final jump target
/// using the `true_condition` to resolve the targets of conditional jumps if possible.
fn find_target_for_retargetable_jump(
    target: &Tid,
    sub: &Sub,
    true_conditions: &[Expression],
) -> Option<Tid> {
    let mut visited_tids = BTreeSet::from([target.clone()]);
    let mut new_target = target;

    while let Some(block) = sub.blocks.iter().find(|blk| blk.tid == *new_target) {
        let Some(retarget) = check_for_retargetable_block(block, true_conditions) else {
            break;
        };

        if !visited_tids.insert(retarget.clone()) {
            // The target was already visited, so we abort the search to avoid infinite loops.
            break;
        }

        new_target = retarget;
    }

    if new_target != target {
        Some(new_target.clone())
    } else {
        None
    }
}

/// Check whether the given block does not contain any [`Def`] instructions.
/// If yes, check whether the target of the jump at the end of the block is predictable
/// under the assumption that the given `true_condition` expression evaluates to true.
/// If it can be predicted, return the target of the jump.
fn check_for_retargetable_block<'a>(
    block: &'a Term<Blk>,
    true_conditions: &[Expression],
) -> Option<&'a Tid> {
    if !block.term.defs.is_empty() {
        return None;
    }

    match &block.term.jmps[..] {
        [Term {
            term: Jmp::Branch(target),
            ..
        }] => Some(target),
        [Term {
            term:
                Jmp::CBranch {
                    target: if_target,
                    condition,
                },
            ..
        }, Term {
            term: Jmp::Branch(else_target),
            ..
        }] => true_conditions.iter().find_map(|true_condition| {
            if condition == true_condition {
                Some(if_target)
            } else if *condition == negate_condition(true_condition.to_owned()) {
                Some(else_target)
            } else {
                None
            }
        }),
        _ => None,
    }
}

/// Returns a condition that we know to be true before the execution of the
/// block.
///
/// Checks whether all edges incoming to the given block are conditioned on the
/// same condition. If true, the shared condition is returned.
fn get_precondition_from_incoming_edges(graph: &Graph, node: NodeIndex) -> Option<Expression> {
    let incoming_edges: Vec<_> = graph
        .edges_directed(node, petgraph::Direction::Incoming)
        .collect();
    let mut first_condition: Option<Expression> = None;

    for edge in incoming_edges.iter() {
        let condition = match edge.weight() {
            Edge::Jump(
                Term {
                    term: Jmp::CBranch { condition, .. },
                    ..
                },
                None,
            ) => condition.clone(),
            Edge::Jump(
                Term {
                    term: Jmp::Branch(_),
                    ..
                },
                Some(Term {
                    term: Jmp::CBranch { condition, .. },
                    ..
                }),
            ) => negate_condition(condition.clone()),
            _ => return None,
        };

        match &mut first_condition {
            // First iteration.
            None => first_condition = Some(condition),
            // Same condition as first incoming edge.
            Some(first_condition) if *first_condition == condition => continue,
            // A different condition implies that we can not make a definitive
            // statement.
            _ => return None,
        }
    }

    first_condition
}

/// Returns a condition that we know to be true after the execution of all DEFs
/// in the block.
///
/// Check if all incoming edges of the given `BlkStart` node are conditioned on
/// the same condition.
/// If yes, check whether the conditional expression will still evaluate to true
/// after the execution of all DEFs of the block.
/// If yes, return the conditional expression.
fn get_block_precondition_after_defs(cfg: &Graph, node: NodeIndex) -> Option<Expression> {
    let Node::BlkStart(block, sub) = cfg[node] else {
        return None;
    };

    if block.tid == sub.term.blocks[0].tid {
        // Function start blocks always have incoming caller edges
        // even if these edges are missing in the CFG because we do not know the callers.
        return None;
    }

    // Check whether we know the result of a conditional at the start of the block
    let block_precondition = get_precondition_from_incoming_edges(cfg, node)?;

    // If we have a known conditional result at the start of the block,
    // check whether it will still hold true at the end of the block.
    let input_vars = block_precondition.input_vars();
    for def in block.term.defs.iter() {
        match &def.term {
            Def::Assign { var, .. } | Def::Load { var, .. } => {
                if input_vars.contains(&var) {
                    return None;
                }
            }
            Def::Store { .. } => (),
        }
    }

    Some(block_precondition)
}

/// Negate the given boolean condition expression, removing double negations in the process.
fn negate_condition(expr: Expression) -> Expression {
    if let Expression::UnOp {
        op: UnOpType::BoolNegate,
        arg,
    } = expr
    {
        *arg
    } else {
        Expression::UnOp {
            op: UnOpType::BoolNegate,
            arg: Box::new(expr),
        }
    }
}

/// Iterates the CFG and returns all node's blocks, that do not have an incoming edge.
fn get_nodes_without_incoming_edge(cfg: &Graph) -> HashSet<Tid> {
    cfg.node_indices()
        .filter_map(|node| {
            if cfg.neighbors_directed(node, Incoming).next().is_none() {
                Some(cfg[node].get_block().tid.clone())
            } else {
                None
            }
        })
        .collect()
}

/// Calculates the difference of the orphaned blocks and removes them from the project.
fn remove_new_orphaned_blocks(
    project: &mut Project,
    orphaned_blocks_before: HashSet<Tid>,
    orphaned_blocks_after: HashSet<Tid>,
) {
    let new_orphan_blocks: HashSet<&Tid> = orphaned_blocks_after
        .difference(&orphaned_blocks_before)
        .collect();
    for sub in project.program.term.subs.values_mut() {
        sub.term
            .blocks
            .retain(|blk| !new_orphan_blocks.contains(&&blk.tid));
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::{def, expr};
    use std::collections::BTreeMap;

    fn mock_condition_block_custom(
        name: &str,
        if_target: &str,
        else_target: &str,
        condition: &str,
    ) -> Term<Blk> {
        let if_jmp = Jmp::CBranch {
            target: Tid::new(if_target),
            condition: expr!(condition),
        };
        let if_jmp = Term {
            tid: Tid::new(name.to_string() + "_jmp_if"),
            term: if_jmp,
        };
        let else_jmp = Jmp::Branch(Tid::new(else_target));
        let else_jmp = Term {
            tid: Tid::new(name.to_string() + "_jmp_else"),
            term: else_jmp,
        };
        let blk = Blk {
            defs: Vec::new(),
            jmps: Vec::from([if_jmp, else_jmp]),
            indirect_jmp_targets: Vec::new(),
        };
        Term {
            tid: Tid::new(name),
            term: blk,
        }
    }

    fn mock_condition_block(name: &str, if_target: &str, else_target: &str) -> Term<Blk> {
        mock_condition_block_custom(name, if_target, else_target, "ZF:1")
    }

    fn mock_jump_only_block(name: &str, return_target: &str) -> Term<Blk> {
        let jmp = Jmp::Branch(Tid::new(return_target));
        let jmp = Term {
            tid: Tid::new(name.to_string() + "_jmp"),
            term: jmp,
        };
        let blk = Blk {
            defs: Vec::new(),
            jmps: vec![jmp],
            indirect_jmp_targets: Vec::new(),
        };
        Term {
            tid: Tid::new(name),
            term: blk,
        }
    }

    fn mock_ret_only_block(name: &str) -> Term<Blk> {
        let ret = Jmp::Return(expr!("0x0:8"));
        let ret = Term {
            tid: Tid::new(name.to_string() + "_ret"),
            term: ret,
        };
        let blk = Blk {
            defs: Vec::new(),
            jmps: vec![ret],
            indirect_jmp_targets: Vec::new(),
        };
        Term {
            tid: Tid::new(name),
            term: blk,
        }
    }

    fn mock_block_with_defs(name: &str, return_target: &str) -> Term<Blk> {
        let def = def![format!("{name}_def: r0:4 = r1:4")];
        let jmp = Jmp::Branch(Tid::new(return_target));
        let jmp = Term {
            tid: Tid::new(name.to_string() + "_jmp"),
            term: jmp,
        };
        let blk = Blk {
            defs: vec![def],
            jmps: vec![jmp],
            indirect_jmp_targets: Vec::new(),
        };
        Term {
            tid: Tid::new(name),
            term: blk,
        }
    }

    fn mock_block_with_defs_and_call(
        name: &str,
        call_target: &str,
        return_target: &str,
    ) -> Term<Blk> {
        let def = def![format!("{name}_def: r0:4 = r1:4")];
        let call = Jmp::Call {
            target: Tid::new(call_target),
            return_: Some(Tid::new(return_target)),
        };
        let call = Term {
            tid: Tid::new(name.to_string() + "_call"),
            term: call,
        };
        let blk = Blk {
            defs: vec![def],
            jmps: vec![call],
            indirect_jmp_targets: Vec::new(),
        };
        Term {
            tid: Tid::new(name),
            term: blk,
        }
    }

    #[test]
    fn test_propagate_control_flow() {
        let sub = Sub {
            name: "sub".to_string(),
            calling_convention: None,
            blocks: vec![
                mock_condition_block("cond_blk_1", "def_blk_1", "cond_blk_2"),
                mock_block_with_defs("def_blk_1", "cond_blk_2"),
                mock_condition_block("cond_blk_2", "def_blk_2", "cond_blk_3"),
                mock_block_with_defs("def_blk_2", "cond_blk_3"),
                mock_condition_block("cond_blk_3", "def_blk_3", "end_blk"),
                mock_block_with_defs("def_blk_3", "end_blk"),
                mock_block_with_defs("end_blk", "end_blk"),
            ],
        };
        let sub = Term {
            tid: Tid::new("sub"),
            term: sub,
        };
        let mut project = Project::mock_arm32();
        project.program.term.subs = BTreeMap::from([(Tid::new("sub"), sub)]);

        propagate_control_flow(&mut project);
        let expected_blocks = vec![
            mock_condition_block("cond_blk_1", "def_blk_1", "end_blk"),
            mock_block_with_defs("def_blk_1", "def_blk_2"),
            // cond_blk_2 removed, since no incoming edge anymore
            mock_block_with_defs("def_blk_2", "def_blk_3"),
            // cond_blk_3 removed, since no incoming edge anymore
            mock_block_with_defs("def_blk_3", "end_blk"),
            mock_block_with_defs("end_blk", "end_blk"),
        ];
        assert_eq!(
            &project.program.term.subs[&Tid::new("sub")].term.blocks[..],
            &expected_blocks[..]
        );
    }

    #[test]
    fn call_return_to_jump() {
        let sub_1 = Sub {
            name: "sub_1".to_string(),
            calling_convention: None,
            blocks: vec![
                mock_block_with_defs_and_call("call_blk", "sub_2", "jump_blk"),
                mock_jump_only_block("jump_blk", "end_blk"),
                mock_block_with_defs("end_blk", "end_blk"),
            ],
        };
        let sub_1 = Term {
            tid: Tid::new("sub_1"),
            term: sub_1,
        };
        let sub_2 = Sub {
            name: "sub_2".to_string(),
            calling_convention: None,
            blocks: vec![mock_ret_only_block("ret_blk")],
        };
        let sub_2 = Term {
            tid: Tid::new("sub_2"),
            term: sub_2,
        };
        let mut project = Project::mock_arm32();
        project.program.term.subs =
            BTreeMap::from([(Tid::new("sub_1"), sub_1), (Tid::new("sub_2"), sub_2)]);

        propagate_control_flow(&mut project);
        let expected_blocks = vec![
            mock_block_with_defs_and_call("call_blk", "sub_2", "end_blk"),
            // jump_blk removed since it has no incoming edges
            mock_block_with_defs("end_blk", "end_blk"),
        ];
        assert_eq!(
            &project.program.term.subs[&Tid::new("sub_1")].term.blocks[..],
            &expected_blocks[..]
        );
    }

    #[test]
    fn call_return_to_cond_jump() {
        let sub_1 = Sub {
            name: "sub_1".to_string(),
            calling_convention: None,
            blocks: vec![
                mock_condition_block("cond_blk_1", "call_blk", "end_blk_1"),
                mock_block_with_defs_and_call("call_blk", "sub_2", "cond_blk_2"),
                mock_condition_block("cond_blk_2", "end_blk_2", "end_blk_1"),
                mock_block_with_defs("end_blk_1", "end_blk_1"),
                mock_block_with_defs("end_blk_2", "end_blk_2"),
            ],
        };
        let sub_1 = Term {
            tid: Tid::new("sub_1"),
            term: sub_1,
        };
        let sub_2 = Sub {
            name: "sub_2".to_string(),
            calling_convention: None,
            blocks: vec![mock_ret_only_block("ret_blk")],
        };
        let sub_2 = Term {
            tid: Tid::new("sub_2"),
            term: sub_2,
        };
        let mut project = Project::mock_arm32();
        project.program.term.subs =
            BTreeMap::from([(Tid::new("sub_1"), sub_1), (Tid::new("sub_2"), sub_2)]);

        propagate_control_flow(&mut project);
        let expected_blocks = vec![
            mock_condition_block("cond_blk_1", "call_blk", "end_blk_1"),
            mock_block_with_defs_and_call("call_blk", "sub_2", "cond_blk_2"),
            // cond_blk_2 can not be skipped as call may modify the inputs to
            // the conditional expresion.
            mock_condition_block("cond_blk_2", "end_blk_2", "end_blk_1"),
            mock_block_with_defs("end_blk_1", "end_blk_1"),
            mock_block_with_defs("end_blk_2", "end_blk_2"),
        ];
        assert_eq!(
            &project.program.term.subs[&Tid::new("sub_1")].term.blocks[..],
            &expected_blocks[..]
        );
    }

    #[test]
    fn multiple_incoming_same_condition() {
        let sub = Sub {
            name: "sub".to_string(),
            calling_convention: None,
            blocks: vec![
                mock_condition_block("cond_blk_1_1", "def_blk_1", "end_blk_1"),
                mock_condition_block("cond_blk_1_2", "def_blk_1", "end_blk_1"),
                mock_block_with_defs("def_blk_1", "cond_blk_2"),
                mock_condition_block("cond_blk_2", "end_blk_2", "end_blk_1"),
                mock_block_with_defs("end_blk_1", "end_blk_1"),
                mock_block_with_defs("end_blk_2", "end_blk_2"),
            ],
        };
        let sub = Term {
            tid: Tid::new("sub"),
            term: sub,
        };
        let mut project = Project::mock_arm32();
        project.program.term.subs = BTreeMap::from([(Tid::new("sub"), sub)]);

        propagate_control_flow(&mut project);
        let expected_blocks = vec![
            mock_condition_block("cond_blk_1_1", "def_blk_1", "end_blk_1"),
            mock_condition_block("cond_blk_1_2", "def_blk_1", "end_blk_1"),
            mock_block_with_defs("def_blk_1", "end_blk_2"),
            // cond_blk_2 removed, since no incoming edge anymore
            mock_block_with_defs("end_blk_1", "end_blk_1"),
            mock_block_with_defs("end_blk_2", "end_blk_2"),
        ];
        assert_eq!(
            &project.program.term.subs[&Tid::new("sub")].term.blocks[..],
            &expected_blocks[..]
        );
    }

    #[test]
    fn multiple_incoming_different_condition() {
        let sub = Sub {
            name: "sub".to_string(),
            calling_convention: None,
            blocks: vec![
                mock_condition_block("cond_blk_1_1", "def_blk_1", "end_blk_1"),
                mock_condition_block("cond_blk_1_2", "end_blk_1", "def_blk_1"),
                mock_block_with_defs("def_blk_1", "cond_blk_2"),
                mock_condition_block("cond_blk_2", "end_blk_2", "end_blk_1"),
                mock_block_with_defs("end_blk_1", "end_blk_1"),
                mock_block_with_defs("end_blk_2", "end_blk_2"),
            ],
        };
        let sub = Term {
            tid: Tid::new("sub"),
            term: sub,
        };
        let mut project = Project::mock_arm32();
        project.program.term.subs = BTreeMap::from([(Tid::new("sub"), sub)]);

        propagate_control_flow(&mut project);
        let expected_blocks = vec![
            mock_condition_block("cond_blk_1_1", "def_blk_1", "end_blk_1"),
            mock_condition_block("cond_blk_1_2", "end_blk_1", "def_blk_1"),
            mock_block_with_defs("def_blk_1", "cond_blk_2"),
            mock_condition_block("cond_blk_2", "end_blk_2", "end_blk_1"),
            mock_block_with_defs("end_blk_1", "end_blk_1"),
            mock_block_with_defs("end_blk_2", "end_blk_2"),
        ];
        assert_eq!(
            &project.program.term.subs[&Tid::new("sub")].term.blocks[..],
            &expected_blocks[..]
        );
    }

    #[test]
    fn multiple_known_conditions() {
        let sub = Sub {
            name: "sub".to_string(),
            calling_convention: None,
            blocks: vec![
                mock_condition_block("cond1_blk_1", "cond2_blk", "end_blk_1"),
                mock_condition_block_custom("cond2_blk", "cond1_blk_2", "end_blk_1", "CF:1"),
                mock_condition_block("cond1_blk_2", "def_blk", "end_blk_1"),
                mock_block_with_defs("def_blk", "end_blk_2"),
                mock_block_with_defs("end_blk_1", "end_blk_1"),
                mock_block_with_defs("end_blk_2", "end_blk_2"),
            ],
        };
        let sub = Term {
            tid: Tid::new("sub"),
            term: sub,
        };
        let mut project = Project::mock_arm32();
        project.program.term.subs = BTreeMap::from([(Tid::new("sub"), sub)]);

        propagate_control_flow(&mut project);
        let expected_blocks = vec![
            mock_condition_block("cond1_blk_1", "cond2_blk", "end_blk_1"),
            mock_condition_block_custom("cond2_blk", "def_blk", "end_blk_1", "CF:1"),
            // removed since no incoming edges
            mock_block_with_defs("def_blk", "end_blk_2"),
            mock_block_with_defs("end_blk_1", "end_blk_1"),
            mock_block_with_defs("end_blk_2", "end_blk_2"),
        ];
        assert_eq!(
            &project.program.term.subs[&Tid::new("sub")].term.blocks[..],
            &expected_blocks[..]
        );
    }
}
