//! The amplification stage that launches an indirect mesh draw: the
//! layer's own, in front of the translated mesh shader, in place of the
//! argument clamp (`victory-gpu`'s `indirect.rs`) and the indirect
//! execution that read the clamped record.
//!
//! One group of one thread per draw. It reads the draw's record (and its
//! count, when the draw has one), applies the clamp's rules for a mesh
//! record clamped alone, charges the list's budget word as the clamp
//! charges it, notes the same two diagnostic words, and launches the mesh
//! groups with `DispatchMesh`. The mesh shader sees what it sees behind
//! the clamp: with one amplification group, its group ids are those of the
//! `DispatchMesh` grid, which is the clamped record's grid.
//!
//! The clamp charges records in the order the list records them, because
//! each clamp pass sits between two waits. Amplification groups of
//! different draws run in no defined order, so the draws take turns: the
//! budget buffer holds the number of the next draw allowed to charge
//! ([`WORD_TURN`]), each draw has its number in the list ([`ticket`]),
//! waits until the turn reaches it, charges and passes the turn on. The
//! wait is bounded ([`TURN_LIMIT`]); a draw that gives up charges out of
//! turn and counts itself ([`WORD_LATE`]), so a run says whether it was
//! exact.
//!
//! The charge itself is a compare-and-exchange loop: read the budget word,
//! work out the charge from it, and store the charge only if the word
//! still holds what was read, else start again from what it holds. So no
//! charge is lost when draws charge at the same time, which only draws out
//! of turn do. The word only grows, so a store fails only after another
//! draw's store succeeded: every draw ends in a bounded number of tries
//! and no limit is needed. A draw out of turn therefore changes only the
//! order of the charges. Every charge adds its cost until the budget is
//! spent, so the budget words are those of the clamp in any order, and
//! while the records of a list fit whole, so are the groups launched; only
//! where the budget runs out can a draw out of turn change which records
//! run empty.
//!
//! Bindings (all in the root signature's amplification part): user data
//! `b0` in [`super::USER_DATA_SPACE`] with two dwords, the ticket and the
//! flags ([`FLAG_COUNT`]); the record as the raw root SRV
//! `t<SLOT_RECORD>`, the count as `t<SLOT_COUNT>` (the record again when
//! there is none), the budget words as the raw root UAV `u<SLOT_BUDGET>`,
//! all three in [`super::ROOT_CBV_SPACE`]. The payload is one unused dword
//! ([`PAYLOAD_BYTES`]); the mesh shader's form behind the stage declares
//! the same ([`super::translate_amplified`]).
//!
//! [`ticket`]: Launch

use super::bitcode::{Attr, Bin, Cast, Term, pred};
use super::shader::Shader;

/// The root SRV of the draw's record (its first byte).
pub const SLOT_RECORD: u8 = 250;
/// The root SRV of the draw's 32-bit record count, if it has one.
pub const SLOT_COUNT: u8 = 251;
/// The root UAV of the list's budget words.
pub const SLOT_BUDGET: u8 = 252;
/// User-data dwords: the ticket, then the flags.
pub const USER_DWORDS: u32 = 2;
/// Flag: a count bounds the draw's one record (zero: nothing runs).
pub const FLAG_COUNT: u32 = 1;
/// Bytes of the payload: one dword nobody reads, the least the DXIL
/// validator measures for a payload. The mesh shader behind the stage must
/// declare the same size (D3D12 mesh shader specification, "DispatchMesh
/// intrinsic"): its translation is made again for that
/// ([`super::translate_amplified`]).
pub const PAYLOAD_BYTES: u32 = 4;

/// The budget word, as the clamp keeps it.
pub const WORD_BUDGET: u32 = 0;
/// The largest group count a mesh record asked for (clamp's word +4).
pub const WORD_LARGEST: u32 = 4;
/// The groups mesh records asked for in all (clamp's word +8).
pub const WORD_ASKED: u32 = 8;
/// The number of the next draw whose turn it is to charge.
pub const WORD_TURN: u32 = 12;
/// Draws that charged out of turn after waiting [`TURN_LIMIT`] times. Not
/// zero: the charges may have come in another order than the list's;
/// none was lost.
pub const WORD_LATE: u32 = 16;
/// Records clamped: x over 65,535, or y or z over 1.
pub const WORD_CLAMPED: u32 = 20;
/// Records with a cost that the budget left empty.
pub const WORD_EMPTIED: u32 = 24;

/// Times a draw reads the turn before it charges out of turn. A draw
/// earlier in the list was launched earlier and charges in a few
/// accesses; the bound keeps a lost turn from stalling the card.
pub const TURN_LIMIT: u32 = 1 << 16;

/// The clamp's budget for a list: 2^27 (vertex instances, threads, mesh
/// groups at 128 each).
pub const BUDGET: u32 = 1 << 27;
/// Groups per dimension a mesh record keeps (D3D12: "less than 64k").
pub const MOST_GROUPS: u32 = 65_535;
/// What a mesh group costs against [`BUDGET`].
pub const GROUP_COST: u32 = 128;
/// Groups at most added to [`WORD_ASKED`] for one record.
pub const MOST_ASKED: u32 = 1 << 24;

/// What one mesh record does, by the clamp's rules for a record clamped
/// alone (no window): the arithmetic the stage computes, for tests and
/// for reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Launch {
    /// The mesh groups launched, per dimension.
    pub groups: [u32; 3],
    /// The budget word afterwards.
    pub budget: u32,
    /// What the record adds to the diagnostic words: the largest asked
    /// (maximum) and the asked groups (sum); zero for no record.
    pub largest: u32,
    pub asked: u32,
}

/// [`Launch`] for the record `record` (x, y, z), `count` (None: no count
/// buffer) and the budget word `before`.
pub fn launch(record: [u32; 3], count: Option<u32>, before: u32) -> Launch {
    if count == Some(0) {
        return Launch {
            groups: [0; 3],
            budget: before,
            largest: 0,
            asked: 0,
        };
    }
    let g = [
        record[0].min(MOST_GROUPS),
        record[1].min(1),
        record[2].min(1),
    ];
    let cost = g[0] * g[1] * g[2] * GROUP_COST;
    let (fits, budget) = if cost == 0 {
        (true, before)
    } else {
        let fits = before < BUDGET && BUDGET - before >= cost;
        let next = if before >= BUDGET || BUDGET - before <= cost {
            BUDGET
        } else {
            before + cost
        };
        (fits, next)
    };
    Launch {
        groups: if fits { g } else { [0; 3] },
        budget,
        largest: record[0],
        asked: record[0].min(MOST_ASKED),
    }
}

/// The stage, a DXIL container (`as_6_6`).
pub fn mesh_launch_as() -> Vec<u8> {
    let mut s = Shader::new(false);
    s.threads = Some([1, 1, 1]);
    s.amplification = Some(PAYLOAD_BYTES);
    s.user_data = USER_DWORDS;
    s.flags |= super::shader::flags::UAVS_EVERY_STAGE;
    let i32t = s.t.i32;
    let entry = s.entry;
    let k = |s: &mut Shader, v: u32| s.m.i32(v);

    let ud = s.user_data_handle();
    let row = k(&mut s, 0);
    let words = s.op(
        entry,
        "cbufferLoadLegacy.i32",
        Attr::ReadOnly,
        s.t.cbuf_ret,
        59,
        &[ud, row],
    );
    let ticket = s.m.extract(entry, words, 0);
    let flags = s.m.extract(entry, words, 1);
    let record = s.root_srv_handle(SLOT_RECORD);
    let count = s.root_srv_handle(SLOT_COUNT);
    let budget = s.root_uav_handle(SLOT_BUDGET);

    // Atomic `op` (DXIL codes: 0 add, 2 or, 7 umax) at byte `at` of the
    // budget words.
    let atomic = |s: &mut Shader, block: usize, op: u32, at: u32, v| {
        let (op, at, iu) = (s.m.i32(op), s.m.i32(at), s.m.undef(i32t));
        s.op(
            block,
            "atomicBinOp.i32",
            Attr::NoUnwind,
            i32t,
            78,
            &[budget, op, at, iu, iu, v],
        )
    };
    // `binary.i32` UMin (40).
    let umin = |s: &mut Shader, block: usize, a, b| {
        s.op(block, "binary.i32", Attr::ReadNone, i32t, 40, &[a, b])
    };

    // The turn: read until it reaches the ticket, or the limit.
    let head = s.m.new_block();
    let after = s.m.new_block();
    s.m.terminate(entry, Term::Br(head));
    let i = s.m.phi(head, i32t);
    let zero = k(&mut s, 0);
    let turn = atomic(&mut s, head, 2, WORD_TURN, zero);
    let ours = s.m.cmp(head, pred::IUGE, turn, ticket);
    let one = k(&mut s, 1);
    let next_i = s.m.bin(head, Bin::Add, i, one);
    let limit = k(&mut s, TURN_LIMIT);
    let spent = s.m.cmp(head, pred::IUGE, next_i, limit);
    let leave = s.m.bin(head, Bin::Or, ours, spent);
    s.m.terminate(head, Term::CondBr(leave, after, head));
    s.m.set_phi(i, vec![(zero, entry), (next_i, head)]);
    let b = after;
    let t = s.m.bool(true);
    let late = s.m.bin(b, Bin::Xor, ours, t);

    // The record, and whether it runs at all (a count of zero: no).
    // (Only the dwords there are: a count may be the last word of its buffer.)
    let raw = |s: &mut Shader, h, mask: u8| {
        let (at, iu, mask, align) = (s.m.i32(0), s.m.undef(i32t), s.m.i8(mask), s.m.i32(4));
        s.op(
            b,
            "rawBufferLoad.f32",
            Attr::ReadOnly,
            s.t.res_ret,
            139,
            &[h, at, iu, mask, align],
        )
    };
    let r = raw(&mut s, record, 7);
    let g: Vec<_> = (0..3)
        .map(|n| {
            let f = s.m.extract(b, r, n);
            s.m.cast(b, Cast::Bitcast, f, i32t)
        })
        .collect();
    let c = raw(&mut s, count, 1);
    let c = s.m.extract(b, c, 0);
    let c = s.m.cast(b, Cast::Bitcast, c, i32t);
    let fc = k(&mut s, FLAG_COUNT);
    let has = s.m.bin(b, Bin::And, flags, fc);
    let no_count = s.m.cmp(b, pred::EQ, has, zero);
    let some = s.m.cmp(b, pred::NE, c, zero);
    let active = s.m.bin(b, Bin::Or, no_count, some);

    // The clamp's rules: x to 65,535, y and z to 1; 128 a group.
    let most = k(&mut s, MOST_GROUPS);
    let gx = umin(&mut s, b, g[0], most);
    let gy = umin(&mut s, b, g[1], one);
    let gz = umin(&mut s, b, g[2], one);
    let yz = s.m.bin(b, Bin::Mul, gy, gz);
    let groups = s.m.bin(b, Bin::Mul, gx, yz);
    let per = k(&mut s, GROUP_COST);
    let cost = s.m.bin(b, Bin::Mul, groups, per);

    // The budget, as the clamp charges it: a record that does not fit
    // whole runs empty and takes the rest. Charged by compare and
    // exchange from the word as read, again from what the word holds
    // until the word still held what was read: a draw charging at the
    // same time (out of turn) never loses a charge. What follows uses the
    // try that stored.
    let first = atomic(&mut s, b, 2, WORD_BUDGET, zero);
    let charge = s.m.new_block();
    s.m.terminate(b, Term::Br(charge));
    let before = s.m.phi(charge, i32t);
    let c = charge;
    let all = k(&mut s, BUDGET);
    let free = s.m.bin(c, Bin::Sub, all, before);
    let under = s.m.cmp(c, pred::IULT, before, all);
    let room = s.m.cmp(c, pred::IUGE, free, cost);
    let fits = s.m.bin(c, Bin::And, under, room);
    let costless = s.m.cmp(c, pred::EQ, cost, zero);
    let fits = s.m.bin(c, Bin::Or, fits, costless);
    let full = s.m.cmp(c, pred::IULE, free, cost);
    let over = s.m.cmp(c, pred::IUGE, before, all);
    let saturates = s.m.bin(c, Bin::Or, full, over);
    let sum = s.m.bin(c, Bin::Add, before, cost);
    let charged = s.m.select(c, saturates, all, sum);
    let charged = s.m.select(c, costless, before, charged);
    let charged = s.m.select(c, active, charged, before);
    let (at, iu) = (k(&mut s, WORD_BUDGET), s.m.undef(i32t));
    let found = s.op(
        c,
        "atomicCompareExchange.i32",
        Attr::NoUnwind,
        i32t,
        79,
        &[budget, at, iu, iu, before, charged],
    );
    let lost = s.m.cmp(c, pred::NE, found, before);
    let b = s.m.new_block();
    s.m.terminate(c, Term::CondBr(lost, charge, b));
    s.m.set_phi(before, vec![(first, after), (found, charge)]);
    // The charge is visible before the turn passes on.
    let fence = k(&mut s, 2);
    s.op(b, "barrier", Attr::NoDuplicate, s.t.void, 80, &[fence]);
    let next = s.m.bin(b, Bin::Add, ticket, one);
    atomic(&mut s, b, 7, WORD_TURN, next);
    // The diagnostic words of the clamp (no record: no change), after
    // the turn: the next draw waits only for the charge.
    let largest = s.m.select(b, active, g[0], zero);
    atomic(&mut s, b, 7, WORD_LARGEST, largest);
    let cap = k(&mut s, MOST_ASKED);
    let asked = umin(&mut s, b, g[0], cap);
    let asked = s.m.select(b, active, asked, zero);
    atomic(&mut s, b, 0, WORD_ASKED, asked);
    let late_n = s.m.cast(b, Cast::ZExt, late, i32t);
    atomic(&mut s, b, 0, WORD_LATE, late_n);
    let wide_x = s.m.cmp(b, pred::IUGT, g[0], most);
    let wide_y = s.m.cmp(b, pred::IUGT, g[1], one);
    let wide_z = s.m.cmp(b, pred::IUGT, g[2], one);
    let wide = s.m.bin(b, Bin::Or, wide_x, wide_y);
    let wide = s.m.bin(b, Bin::Or, wide, wide_z);
    let wide = s.m.bin(b, Bin::And, wide, active);
    let wide = s.m.cast(b, Cast::ZExt, wide, i32t);
    atomic(&mut s, b, 0, WORD_CLAMPED, wide);
    let empty = s.m.bin(b, Bin::Xor, fits, t);
    let empty = s.m.bin(b, Bin::And, empty, active);
    let empty = s.m.cast(b, Cast::ZExt, empty, i32t);
    atomic(&mut s, b, 0, WORD_EMPTIED, empty);

    // The groups: the clamped record's, or none.
    let run = s.m.bin(b, Bin::And, active, fits);
    let dims = [gx, gy, gz].map(|d| s.m.select(b, run, d, zero));
    let payload_ty = s.m.named_struct("struct.Payload", vec![i32t]);
    let payload = s.m.global("payload", payload_ty, 3);
    s.op(
        b,
        "dispatchMesh.struct.Payload",
        Attr::NoUnwind,
        s.t.void,
        173,
        &[dims[0], dims[1], dims[2], payload],
    );
    s.m.terminate(b, Term::Ret);
    s.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_launch_as_the_clamp_clamps_them_alone() {
        // Ordinary, none, the limit, over it, all ones, y and z.
        assert_eq!(
            launch([7, 1, 1], None, 0),
            Launch {
                groups: [7, 1, 1],
                budget: 7 * 128,
                largest: 7,
                asked: 7
            }
        );
        assert_eq!(
            launch([0, 1, 1], None, 5),
            Launch {
                groups: [0, 1, 1],
                budget: 5,
                largest: 0,
                asked: 0
            }
        );
        assert_eq!(launch([65_535, 1, 1], None, 0).groups, [65_535, 1, 1]);
        let over = launch([65_536, 1, 1], None, 0);
        assert_eq!(
            (over.groups, over.budget, over.largest, over.asked),
            ([65_535, 1, 1], 65_535 * 128, 65_536, 65_536)
        );
        let ones = launch([u32::MAX; 3], None, 0);
        assert_eq!(
            (ones.groups, ones.largest, ones.asked),
            ([65_535, 1, 1], u32::MAX, 1 << 24)
        );
        assert_eq!(launch([3, 5, 9], None, 0).groups, [3, 1, 1]);
        // A zero y launches nothing and costs nothing, but is asked.
        assert_eq!(
            launch([3, 0, 1], None, 0),
            Launch {
                groups: [3, 0, 1],
                budget: 0,
                largest: 3,
                asked: 3
            }
        );
        // A count: zero runs nothing and notes nothing; more runs the record.
        assert_eq!(
            launch([9, 1, 1], Some(0), 11),
            Launch {
                groups: [0; 3],
                budget: 11,
                largest: 0,
                asked: 0
            }
        );
        assert_eq!(launch([9, 1, 1], Some(4), 0).groups, [9, 1, 1]);
    }

    #[test]
    fn the_budget_saturates_as_the_clamp_charges_it() {
        // Exactly the rest: runs, and the budget is spent.
        let rest = BUDGET - 10 * 128;
        assert_eq!(
            launch([10, 1, 1], None, rest),
            Launch {
                groups: [10, 1, 1],
                budget: BUDGET,
                largest: 10,
                asked: 10
            }
        );
        // One group more than the rest: empty, and the budget is spent.
        assert_eq!(launch([11, 1, 1], None, rest).groups, [0; 3]);
        assert_eq!(launch([11, 1, 1], None, rest).budget, BUDGET);
        // Spent: every record with a cost runs empty, costless ones run.
        assert_eq!(launch([1, 1, 1], None, BUDGET).groups, [0; 3]);
        assert_eq!(launch([0, 1, 1], None, BUDGET).groups, [0, 1, 1]);
        // In a row: 16 records of 65,535 groups fit (16 * 65,535 * 128 <
        // 2^27); the 17th does not, nor a small one after it.
        let mut at = 0;
        for _ in 0..16 {
            let l = launch([65_535, 1, 1], None, at);
            assert_eq!(l.groups, [65_535, 1, 1]);
            at = l.budget;
        }
        assert_eq!(launch([65_535, 1, 1], None, at).groups, [0; 3]);
        let after = launch([65_535, 1, 1], None, at).budget;
        assert_eq!(after, BUDGET);
        assert_eq!(launch([1, 1, 1], None, after).groups, [0; 3]);
    }

    #[test]
    fn the_stage_is_an_amplification_shader() {
        let c = mesh_launch_as();
        let parsed = crate::dxbc::Container::parse(&c).expect("a container");
        let dxil = parsed.part(crate::dxbc::FourCC::DXIL).expect("a DXIL part");
        // Program kind 14 (amplification), shader model 6.6.
        assert_eq!(
            u32::from_le_bytes(dxil.data[0..4].try_into().unwrap()),
            14 << 16 | 0x66
        );
    }

    #[cfg(windows)]
    #[test]
    fn the_stage_validates() {
        let path = super::super::validator::Validator::default_path();
        match super::super::validator::Validator::load(path.as_deref()) {
            Ok(mut v) => v
                .validate(&mut mesh_launch_as())
                .expect("DXIL validation failed"),
            Err(e) => eprintln!("SKIPPED (cargo counts it as passed): the_stage_validates: {e}"),
        }
    }

    /// A mesh shader finished inside `with_mesh_payload` declares the
    /// payload (metadata and `PSV0`) and still validates; outside, none.
    #[cfg(windows)]
    #[test]
    fn a_mesh_shader_declares_the_payload_only_when_asked() {
        let mesh = || {
            let mut s = Shader::new(false);
            s.threads = Some([1, 1, 1]);
            s.mesh = Some(super::super::shader::Mesh {
                max_vertices: 3,
                max_primitives: 1,
                topology: 2,
            });
            let (v, p) = (s.m.i32(0), s.m.i32(0));
            let void = s.t.void;
            let e = s.entry;
            s.op(e, "setMeshOutputCounts", Attr::NoUnwind, void, 168, &[v, p]);
            s.m.terminate(e, Term::Ret);
            s.finish()
        };
        let plain = mesh();
        let amplified = super::super::shader::with_mesh_payload(PAYLOAD_BYTES, mesh);
        assert_eq!(mesh(), plain, "the payload stays off outside");
        let payload = |c: &[u8]| {
            let parsed = crate::dxbc::Container::parse(c).expect("a container");
            let psv = parsed.part(crate::dxbc::FourCC::PSV0).expect("PSV0").data;
            // After the runtime info's size: MSInfo.PayloadSizeInBytes at +8.
            u32::from_le_bytes(psv[12..16].try_into().unwrap())
        };
        assert_eq!((payload(&plain), payload(&amplified)), (0, PAYLOAD_BYTES));
        let path = super::super::validator::Validator::default_path();
        match super::super::validator::Validator::load(path.as_deref()) {
            Ok(mut v) => {
                v.validate(&mut plain.clone())
                    .expect("the plain form validates");
                v.validate(&mut amplified.clone())
                    .expect("the form with the payload validates");
            }
            Err(e) => eprintln!(
                "SKIPPED (cargo counts it as passed): a_mesh_shader_declares_the_payload_only_when_asked: {e}"
            ),
        }
    }
}
