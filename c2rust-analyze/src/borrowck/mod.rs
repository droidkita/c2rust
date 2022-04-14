use std::collections::HashMap;
use std::env;
use std::hash::Hash;
use std::mem;
use either::{Either, Left, Right};
use polonius_engine::{self, Atom, FactTypes};
use rustc_ast::ast::{Item, ItemKind, Visibility, VisibilityKind};
use rustc_ast::node_id::NodeId;
use rustc_ast::ptr::P;
use rustc_driver::Compilation;
use rustc_interface::Queries;
use rustc_interface::interface::Compiler;
use rustc_middle::mir::{
    Body, BasicBlock, BasicBlockData, START_BLOCK, Terminator, TerminatorKind, SourceInfo, Local,
    LocalDecl, LocalKind, Mutability, Rvalue, AggregateKind, Place, Operand, Statement,
    StatementKind, BorrowKind, Constant, ConstantKind,
};
use rustc_middle::mir::interpret::{Allocation, ConstValue};
use rustc_middle::mir::pretty;
use rustc_middle::ty::{TyCtxt, Ty, TyKind, RegionKind, WithOptConstParam, List};
use rustc_middle::ty::query::{Providers, ExternProviders};
use rustc_session::Session;
use rustc_span::DUMMY_SP;
use rustc_span::def_id::{DefId, LocalDefId, CRATE_DEF_INDEX};
use rustc_span::symbol::Ident;
use rustc_target::abi::Align;
use crate::context::{AnalysisCtxt, PermissionSet, PointerId};
use crate::dataflow::DataflowConstraints;
use crate::labeled_ty::{LabeledTy, LabeledTyCtxt};
use self::atoms::{AllFacts, AtomMaps, Output, SubPoint, Origin, Path, Loan};


mod atoms;
mod def_use;
mod dump;
mod type_check;


#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash, Default)]
struct Label {
    origin: Option<Origin>,
    perm: PermissionSet,
}

pub type LTy<'tcx> = LabeledTy<'tcx, Label>;
pub type LTyCtxt<'tcx> = LabeledTyCtxt<'tcx, Label>;


pub fn borrowck_mir<'tcx>(
    acx: &AnalysisCtxt<'tcx>,
    dataflow: &DataflowConstraints,
    hypothesis: &mut [PermissionSet],
    name: &str,
    mir: &Body<'tcx>,
) {
    let mut i = 0;
    loop {
        eprintln!("run polonius");
        let (facts, maps, output) = run_polonius(acx, hypothesis, name, mir);
        eprintln!("polonius: iteration {}: {} errors", i, output.errors.len());
        i += 1;

        if output.errors.len() == 0 {
            break;
        }
        if i >= 20 { panic!() }

        let mut changed = false;
        for (_, loans) in &output.errors {
            for &loan in loans {
                let issued_point = facts.loan_issued_at.iter().find(|&&(_, l, _)| l == loan)
                    .map(|&(_, _, point)| point)
                    .unwrap_or_else(|| panic!("loan {:?} was never issued?", loan));
                let issued_loc = maps.get_point_location(issued_point);
                let stmt = mir.stmt_at(issued_loc).left().unwrap_or_else(|| {
                    panic!("loan {:?} was issued by a terminator (at {:?})?", loan, issued_loc);
                });
                // TODO:
                // - address of local: adjust `addr_of_local[l]`
                // - address of deref + project: adjust deref'd operand
                // - copy/move: adjust copied ptr
                let pl = match stmt.kind {
                    StatementKind::Assign(ref x) => {
                        match x.1 {
                            Rvalue::Use(_) => todo!(),
                            Rvalue::Ref(_, _, pl) => pl,
                            Rvalue::AddressOf(_, pl) => pl,
                            // TODO: handle direct assignment from another pointer
                            ref rv => panic!(
                                "loan {:?} was issued by unknown rvalue {:?}?", loan, rv,
                            ),
                        }
                    },
                    _ => panic!("loan {:?} was issued by non-assign stmt {:?}?", loan, stmt),
                };
                eprintln!("want to drop UNIQUE from place {:?}", pl);

                let ptr = if let Some(l) = pl.as_local() {
                    acx.addr_of_local[l]
                } else {
                    todo!();
                };

                if hypothesis[ptr.index()].contains(PermissionSet::UNIQUE) {
                    hypothesis[ptr.index()].remove(PermissionSet::UNIQUE);
                    changed = true;
                }
            }
        }

        eprintln!("propagate");
        changed |= dataflow.propagate(hypothesis);
        eprintln!("done propagating");

        if !changed {
            eprintln!(
                "{} unresolved borrowck errors in function {:?} (after {} iterations)",
                output.errors.len(),
                name,
                i,
            );
            break;
        }
    }

    eprintln!("final labeling for {:?}:", name);
    let lcx2 = crate::labeled_ty::LabeledTyCtxt::new(acx.tcx);
    for (local, _) in mir.local_decls.iter_enumerated() {
        let addr_of = hypothesis[acx.addr_of_local[local].index()];
        let ty = lcx2.relabel(acx.local_tys[local], &mut |lty| {
            if lty.label == PointerId::NONE {
                PermissionSet::empty()
            } else {
                hypothesis[lty.label.index()]
            }
        });
        eprintln!("{:?}: addr_of = {:?}, type = {:?}", local, addr_of, ty);
    }
}


fn run_polonius<'tcx>(
    acx: &AnalysisCtxt<'tcx>,
    hypothesis: &[PermissionSet],
    name: &str,
    mir: &Body<'tcx>,
) -> (AllFacts, AtomMaps<'tcx>, Output) {
    let mut facts = AllFacts::default();
    let mut maps = AtomMaps::default();

    // Start the origin counter at 3.  This has no effect on the semantics, but makes for easier
    // diffs between our facts and the facts generated by rustc.
    for _ in 0..3 {
        let _ = maps.origin();
    }

    //pretty::write_mir_fn(tcx, mir, &mut |_, _| Ok(()), &mut std::io::stdout()).unwrap();

    // Populate `cfg_edge`
    for (bb, bb_data) in mir.basic_blocks().iter_enumerated() {
        eprintln!("{:?}:", bb);

        for idx in 0 .. bb_data.statements.len() {
            eprintln!("  {}: {:?}", idx, bb_data.statements[idx]);
            let start = maps.point(bb, idx, SubPoint::Start);
            let mid = maps.point(bb, idx, SubPoint::Mid);
            let next_start = maps.point(bb, idx + 1, SubPoint::Start);
            facts.cfg_edge.push((start, mid));
            facts.cfg_edge.push((mid, next_start));
        }

        let term_idx = bb_data.statements.len();
        eprintln!("  {}: {:?}", term_idx, bb_data.terminator());
        let term_start = maps.point(bb, term_idx, SubPoint::Start);
        let term_mid = maps.point(bb, term_idx, SubPoint::Mid);
        facts.cfg_edge.push((term_start, term_mid));
        for &succ in bb_data.terminator().successors() {
            let succ_start = maps.point(succ, 0, SubPoint::Start);
            facts.cfg_edge.push((term_mid, succ_start));
        }
    }

    // From rustc_borrowck::nll::populate_polonius_move_facts: "Non-arguments start out
    // deinitialised; we simulate this with an initial move"
    let entry_point = maps.point(START_BLOCK, 0, SubPoint::Start);
    for local in mir.local_decls.indices() {
        if mir.local_kind(local) != LocalKind::Arg {
            let path = maps.path(&mut facts, Place { local, projection: List::empty() });
            facts.path_moved_at_base.push((path, entry_point));
        }
    }

    // Populate `use_of_var_derefs_origin`, and generate `LTy`s for all locals.
    let ltcx = LabeledTyCtxt::new(acx.tcx);
    let mut local_ltys = Vec::with_capacity(mir.local_decls.len());
    for local in mir.local_decls.indices() {
        let lty = assign_origins(ltcx, hypothesis, &mut facts, &mut maps, acx.local_tys[local]);
        let var = maps.variable(local);
        lty.for_each_label(&mut |label| {
            if let Some(origin) = label.origin {
                facts.use_of_var_derefs_origin.push((var, origin));
            }
        });
        local_ltys.push(lty);
    }

    let mut loans = HashMap::<Local, Vec<(Path, Loan, BorrowKind)>>::new();
    // Populate `loan_issued_at` and `loans`.
    type_check::visit(ltcx, &mut facts, &mut maps, &mut loans, &local_ltys, mir);

    // Populate `loan_invalidated_at`
    def_use::visit_loan_invalidated_at(acx.tcx, &mut facts, &mut maps, &loans, mir);

    // Populate `var_defined/used/dropped_at` and `path_assigned/accessed_at_base`.
    def_use::visit(&mut facts, &mut maps, mir);


    dump::dump_facts_to_dir(&facts, &maps, format!("inspect/{}", name)).unwrap();

    let output = polonius_engine::Output::compute(
        &facts,
        polonius_engine::Algorithm::Naive,
        true,
    );
    dump::dump_output_to_dir(&output, &maps, format!("inspect/{}", name)).unwrap();

    (facts, maps, output)
}

fn assign_origins<'tcx>(
    ltcx: LTyCtxt<'tcx>,
    hypothesis: &[PermissionSet],
    facts: &mut AllFacts,
    maps: &mut AtomMaps<'tcx>,
    lty: crate::LTy<'tcx>,
) -> LTy<'tcx> {
    ltcx.relabel(lty, &mut |lty| {
        let perm = if lty.label.is_none() {
            PermissionSet::empty()
        } else {
            hypothesis[lty.label.index()]
        };
        match lty.ty.kind() {
            TyKind::Ref(_, _, _) |
            TyKind::RawPtr(_) => {
                let origin = Some(maps.origin());
                Label { origin, perm }
            },
            _ => Label { origin: None, perm },
        }
    })
}
