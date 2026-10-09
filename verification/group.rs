use vstd::prelude::*;

verus! {

// Model of `src/group.rs`. The store thread adds queued writes to one
// group commit while the group fits one WAL block. The check must give the
// true answer of the unbounded sum, and it must never overflow.

pub fn fits(
    bytes: usize,
    ops: usize,
    add_bytes: usize,
    add_ops: usize,
    max_bytes: usize,
    max_ops: usize,
) -> (r: bool)
    ensures
        r <==> (bytes as int + add_bytes as int <= max_bytes as int
            && ops as int + add_ops as int <= max_ops as int),
{
    bytes <= max_bytes && add_bytes <= max_bytes - bytes && ops <= max_ops && add_ops
        <= max_ops - ops
}

fn main() {}

} // verus!
