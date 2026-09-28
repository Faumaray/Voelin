//! Incremental model updates.

use slint::{Model, VecModel};

/// Row operations [`sync`] applied (for tests and tracing).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Changes {
	pub set: usize,
	pub inserted: usize,
	pub removed: usize,
}

/// Make `model` hold `new` with few row operations: the rows both share at
/// the start and the end stay, rows in between are changed in place where
/// they differ, and the rest inserted or removed. Views redraw only the
/// rows that changed (a client that starts talking is one `set_row_data`).
pub fn sync<T: Clone + PartialEq + 'static>(model: &VecModel<T>, new: &[T]) -> Changes {
	let old_len = model.row_count();
	let new_len = new.len();
	let min = old_len.min(new_len);
	let mut prefix = 0;
	while prefix < min && model.row_data(prefix).as_ref() == Some(&new[prefix]) {
		prefix += 1;
	}
	let mut suffix = 0;
	while suffix < min - prefix
		&& model.row_data(old_len - 1 - suffix).as_ref() == Some(&new[new_len - 1 - suffix])
	{
		suffix += 1;
	}
	let old_mid = old_len - prefix - suffix;
	let new_mid = new_len - prefix - suffix;
	let common = old_mid.min(new_mid);
	let mut changes = Changes::default();
	for (i, row) in new.iter().enumerate().skip(prefix).take(common) {
		if model.row_data(i).as_ref() != Some(row) {
			model.set_row_data(i, row.clone());
			changes.set += 1;
		}
	}
	let at = prefix + common;
	for _ in new_mid..old_mid {
		model.remove(at);
		changes.removed += 1;
	}
	for (k, row) in new[at..at + new_mid.saturating_sub(old_mid)].iter().enumerate() {
		model.insert(at + k, row.clone());
		changes.inserted += 1;
	}
	changes
}

#[cfg(test)]
mod tests {
	use super::*;

	fn run(old: &[i32], new: &[i32]) -> Changes {
		let model = VecModel::from(old.to_vec());
		let changes = sync(&model, new);
		assert_eq!(model.iter().collect::<Vec<_>>(), new, "{old:?} -> {new:?}");
		changes
	}

	#[test]
	fn minimal_changes() {
		assert_eq!(run(&[1, 2, 3], &[1, 2, 3]), Changes::default());
		assert_eq!(run(&[1, 2, 3], &[1, 9, 3]), Changes { set: 1, ..Default::default() });
		assert_eq!(run(&[1, 2, 3], &[1, 2, 7, 3]), Changes { inserted: 1, ..Default::default() });
		assert_eq!(run(&[1, 2, 3], &[1, 3]), Changes { removed: 1, ..Default::default() });
		assert_eq!(run(&[], &[1, 2]), Changes { inserted: 2, ..Default::default() });
		assert_eq!(run(&[1, 2], &[]), Changes { removed: 2, ..Default::default() });
		assert_eq!(run(&[1, 2, 3, 4], &[5, 6]).set, 2);
		run(&[1, 1, 1], &[1, 1]);
		run(&[1, 2, 1], &[1, 1, 2, 1]);
		run(&[4, 5, 6], &[1, 2, 3, 4, 5, 6, 7]);
	}
}
