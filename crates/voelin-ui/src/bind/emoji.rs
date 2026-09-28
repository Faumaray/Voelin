//! Emoji: images for the `Images` global, and the picker's rows.

use std::rc::Rc;

use slint::{ComponentHandle, Model, ModelRc, VecModel};

use crate::app::{Emoji, EmojiCell, EmojiRow, Images, MainWindow};
use crate::emoji::{self, PickerEmoji};
use crate::images;

/// Rows of `columns` cells.
fn rows(emoji: Vec<PickerEmoji>, columns: usize) -> Vec<EmojiRow> {
	emoji
		.chunks(columns.max(1))
		.map(|chunk| EmojiRow {
			cells: ModelRc::from(Rc::new(VecModel::from(
				chunk
					.iter()
					.map(|e| EmojiCell {
						key: e.key.into(),
						name: e.name.into(),
						text: e.text.clone().into(),
					})
					.collect::<Vec<_>>(),
			))),
		})
		.collect()
}

fn show(ui: &MainWindow, category: i32, list: Vec<PickerEmoji>) {
	let global = ui.global::<Emoji>();
	let columns = usize::try_from(global.get_columns()).unwrap_or(8);
	global.set_category(category);
	global.set_rows(ModelRc::from(Rc::new(VecModel::from(rows(list, columns)))));
}

pub(super) fn wire(ui: &MainWindow) {
	ui.global::<Images>().on_emoji(|key| images::emoji(&key));
	let global = ui.global::<Emoji>();
	let weak = ui.as_weak();
	global.on_load(move || {
		if let Some(ui) = weak.upgrade()
			&& ui.global::<Emoji>().get_rows().row_count() == 0
		{
			let category = ui.global::<Emoji>().get_category().max(0);
			show(&ui, category, emoji::archive().category(category as usize));
		}
	});
	let weak = ui.as_weak();
	global.on_show_category(move |category| {
		if let Some(ui) = weak.upgrade() {
			show(&ui, category, emoji::archive().category(category.max(0) as usize));
		}
	});
	let weak = ui.as_weak();
	global.on_search(move |text| {
		let Some(ui) = weak.upgrade() else { return };
		if text.trim().is_empty() {
			let category = ui.global::<Emoji>().get_category().max(0);
			show(&ui, category, emoji::archive().category(category as usize));
		} else {
			show(&ui, -1, emoji::archive().search(&text));
		}
	});
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn rows_of_cells() {
		let list = emoji::archive().category(0);
		let n = list.len();
		let rows = rows(list, 8);
		assert_eq!(rows.len(), n.div_ceil(8));
		assert_eq!(rows[0].cells.row_count(), 8);
		assert_eq!(rows[0].cells.row_data(0).unwrap().key, "1f600");
	}
}
