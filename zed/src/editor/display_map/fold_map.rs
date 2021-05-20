use super::{
    buffer::{AnchorRangeExt, TextSummary},
    Anchor, Bias, Buffer, DisplayPoint, Edit, Point, ToOffset,
};
use crate::{
    editor::buffer,
    sum_tree::{self, Cursor, FilterCursor, SeekBias, SumTree},
    time,
};
use gpui::{AppContext, ModelHandle};
use parking_lot::{Mutex, MutexGuard};
use std::{
    cmp::{self, Ordering},
    ops::Range,
};

pub struct FoldMap {
    buffer: ModelHandle<Buffer>,
    transforms: Mutex<SumTree<Transform>>,
    folds: SumTree<Fold>,
    last_sync: Mutex<time::Global>,
}

impl FoldMap {
    pub fn new(buffer_handle: ModelHandle<Buffer>, ctx: &AppContext) -> Self {
        let buffer = buffer_handle.read(ctx);
        let text_summary = buffer.text_summary();
        Self {
            buffer: buffer_handle,
            folds: Default::default(),
            transforms: Mutex::new(SumTree::from_item(
                Transform {
                    summary: TransformSummary {
                        buffer: text_summary.clone(),
                        display: text_summary,
                    },
                    display_text: None,
                },
                &(),
            )),
            last_sync: Mutex::new(buffer.version()),
        }
    }

    pub fn snapshot(&self, ctx: &AppContext) -> FoldMapSnapshot {
        FoldMapSnapshot {
            transforms: self.sync(ctx).clone(),
            buffer: self.buffer.clone(),
        }
    }

    pub fn len(&self, ctx: &AppContext) -> usize {
        self.sync(ctx).summary().display.bytes
    }

    pub fn line_len(&self, row: u32, ctx: &AppContext) -> u32 {
        let line_start = self.to_display_offset(DisplayPoint::new(row, 0), ctx).0;
        let line_end = if row >= self.max_point(ctx).row() {
            self.len(ctx)
        } else {
            self.to_display_offset(DisplayPoint::new(row + 1, 0), ctx).0 - 1
        };
        (line_end - line_start) as u32
    }

    pub fn max_point(&self, ctx: &AppContext) -> DisplayPoint {
        DisplayPoint(self.sync(ctx).summary().display.lines)
    }

    pub fn rightmost_row(&self, ctx: &AppContext) -> u32 {
        self.sync(ctx).summary().display.rightmost_point.row
    }

    pub fn folds_in_range<'a, T>(
        &'a self,
        range: Range<T>,
        ctx: &'a AppContext,
    ) -> impl Iterator<Item = &'a Range<Anchor>>
    where
        T: ToOffset,
    {
        self.intersecting_folds(range, ctx).map(|f| &f.0)
    }

    pub fn fold<T: ToOffset>(
        &mut self,
        ranges: impl IntoIterator<Item = Range<T>>,
        ctx: &AppContext,
    ) {
        let _ = self.sync(ctx);

        let mut edits = Vec::new();
        let mut folds = Vec::new();
        let buffer = self.buffer.read(ctx);
        for range in ranges.into_iter() {
            let range = range.start.to_offset(buffer)..range.end.to_offset(buffer);
            if range.start != range.end {
                let fold = Fold(buffer.anchor_after(range.start)..buffer.anchor_before(range.end));
                folds.push(fold);
                edits.push(Edit {
                    old_range: range.clone(),
                    new_range: range.clone(),
                });
            }
        }

        folds.sort_unstable_by(|a, b| sum_tree::SeekDimension::cmp(a, b, buffer));
        edits.sort_unstable_by(|a, b| {
            a.old_range
                .start
                .cmp(&b.old_range.start)
                .then_with(|| b.old_range.end.cmp(&a.old_range.end))
        });

        self.folds = {
            let mut new_tree = SumTree::new();
            let mut cursor = self.folds.cursor::<_, ()>();
            for fold in folds {
                new_tree.push_tree(cursor.slice(&fold, SeekBias::Right, buffer), buffer);
                new_tree.push(fold, buffer);
            }
            new_tree.push_tree(cursor.suffix(buffer), buffer);
            new_tree
        };
        self.apply_edits(edits, ctx);
    }

    pub fn unfold<T: ToOffset>(
        &mut self,
        ranges: impl IntoIterator<Item = Range<T>>,
        ctx: &AppContext,
    ) {
        let _ = self.sync(ctx);

        let buffer = self.buffer.read(ctx);

        let mut edits = Vec::new();
        let mut fold_ixs_to_delete = Vec::new();
        for range in ranges.into_iter() {
            // Remove intersecting folds and add their ranges to edits that are passed to apply_edits.
            let mut folds_cursor = self.intersecting_folds(range, ctx);
            while let Some(fold) = folds_cursor.item() {
                let offset_range = fold.0.start.to_offset(buffer)..fold.0.end.to_offset(buffer);
                edits.push(Edit {
                    old_range: offset_range.clone(),
                    new_range: offset_range,
                });
                fold_ixs_to_delete.push(*folds_cursor.start());
                folds_cursor.next();
            }
        }

        fold_ixs_to_delete.sort_unstable();
        fold_ixs_to_delete.dedup();
        edits.sort_unstable_by(|a, b| {
            a.old_range
                .start
                .cmp(&b.old_range.start)
                .then_with(|| b.old_range.end.cmp(&a.old_range.end))
        });

        self.folds = {
            let mut cursor = self.folds.cursor::<_, ()>();
            let mut folds = SumTree::new();
            for fold_ix in fold_ixs_to_delete {
                folds.push_tree(cursor.slice(&fold_ix, SeekBias::Right, buffer), buffer);
                cursor.next();
            }
            folds.push_tree(cursor.suffix(buffer), buffer);
            folds
        };
        self.apply_edits(edits, ctx);
    }

    fn intersecting_folds<'a, T>(
        &self,
        range: Range<T>,
        ctx: &'a AppContext,
    ) -> FilterCursor<impl 'a + Fn(&FoldSummary) -> bool, Fold, usize>
    where
        T: ToOffset,
    {
        let buffer = self.buffer.read(ctx);
        let start = buffer.anchor_before(range.start.to_offset(buffer));
        let end = buffer.anchor_after(range.end.to_offset(buffer));
        self.folds.filter::<_, usize>(move |summary| {
            start.cmp(&summary.max_end, buffer).unwrap() == Ordering::Less
                && end.cmp(&summary.min_start, buffer).unwrap() == Ordering::Greater
        })
    }

    pub fn is_line_folded(&self, display_row: u32, ctx: &AppContext) -> bool {
        let transforms = self.sync(ctx);
        let mut cursor = transforms.cursor::<DisplayPoint, DisplayPoint>();
        cursor.seek(&DisplayPoint::new(display_row, 0), SeekBias::Right, &());
        while let Some(transform) = cursor.item() {
            if transform.display_text.is_some() {
                return true;
            }
            if cursor.end().row() == display_row {
                cursor.next()
            } else {
                break;
            }
        }
        false
    }

    pub fn to_buffer_offset(&self, point: DisplayPoint, ctx: &AppContext) -> usize {
        self.snapshot(ctx).to_buffer_offset(point, ctx)
    }

    pub fn to_display_offset(&self, point: DisplayPoint, ctx: &AppContext) -> DisplayOffset {
        self.snapshot(ctx).to_display_offset(point, ctx)
    }

    pub fn to_buffer_point(&self, display_point: DisplayPoint, ctx: &AppContext) -> Point {
        let transforms = self.sync(ctx);
        let mut cursor = transforms.cursor::<DisplayPoint, TransformSummary>();
        cursor.seek(&display_point, SeekBias::Right, &());
        let overshoot = display_point.0 - cursor.start().display.lines;
        cursor.start().buffer.lines + overshoot
    }

    pub fn to_display_point(&self, point: Point, ctx: &AppContext) -> DisplayPoint {
        let transforms = self.sync(ctx);
        let mut cursor = transforms.cursor::<Point, TransformSummary>();
        cursor.seek(&point, SeekBias::Right, &());
        let overshoot = point - cursor.start().buffer.lines;
        DisplayPoint(cmp::min(
            cursor.start().display.lines + overshoot,
            cursor.end().display.lines,
        ))
    }

    fn sync(&self, ctx: &AppContext) -> MutexGuard<SumTree<Transform>> {
        let buffer = self.buffer.read(ctx);
        let mut edits = buffer.edits_since(self.last_sync.lock().clone()).peekable();
        if edits.peek().is_some() {
            self.apply_edits(edits, ctx);
        }
        *self.last_sync.lock() = buffer.version();
        self.transforms.lock()
    }

    fn apply_edits(&self, edits: impl IntoIterator<Item = Edit>, ctx: &AppContext) {
        let buffer = self.buffer.read(ctx);
        let mut edits = edits.into_iter().peekable();

        let mut new_transforms = SumTree::new();
        let mut transforms = self.transforms.lock();
        let mut cursor = transforms.cursor::<usize, usize>();
        cursor.seek(&0, SeekBias::Right, &());

        while let Some(mut edit) = edits.next() {
            new_transforms.push_tree(
                cursor.slice(&edit.old_range.start, SeekBias::Left, &()),
                &(),
            );
            edit.new_range.start -= edit.old_range.start - cursor.start();
            edit.old_range.start = *cursor.start();

            cursor.seek(&edit.old_range.end, SeekBias::Right, &());
            cursor.next();

            let mut delta = edit.delta();
            loop {
                edit.old_range.end = *cursor.start();

                if let Some(next_edit) = edits.peek() {
                    if next_edit.old_range.start > edit.old_range.end {
                        break;
                    }

                    let next_edit = edits.next().unwrap();
                    delta += next_edit.delta();

                    if next_edit.old_range.end >= edit.old_range.end {
                        edit.old_range.end = next_edit.old_range.end;
                        cursor.seek(&edit.old_range.end, SeekBias::Right, &());
                        cursor.next();
                    }
                } else {
                    break;
                }
            }

            edit.new_range.end =
                ((edit.new_range.start + edit.old_extent()) as isize + delta) as usize;

            let anchor = buffer.anchor_before(edit.new_range.start);
            let mut folds_cursor = self.folds.cursor::<_, ()>();
            folds_cursor.seek(&Fold(anchor..Anchor::End), SeekBias::Left, buffer);
            let mut folds = folds_cursor
                .map(|f| f.0.start.to_offset(buffer)..f.0.end.to_offset(buffer))
                .peekable();

            while folds
                .peek()
                .map_or(false, |fold| fold.start < edit.new_range.end)
            {
                let mut fold = folds.next().unwrap();
                let sum = new_transforms.summary();

                assert!(fold.start >= sum.buffer.bytes);

                while folds
                    .peek()
                    .map_or(false, |next_fold| next_fold.start <= fold.end)
                {
                    let next_fold = folds.next().unwrap();
                    if next_fold.end > fold.end {
                        fold.end = next_fold.end;
                    }
                }

                if fold.start > sum.buffer.bytes {
                    let text_summary = buffer.text_summary_for_range(sum.buffer.bytes..fold.start);
                    new_transforms.push(
                        Transform {
                            summary: TransformSummary {
                                display: text_summary.clone(),
                                buffer: text_summary,
                            },
                            display_text: None,
                        },
                        &(),
                    );
                }

                if fold.end > fold.start {
                    let display_text = "…";
                    let chars = display_text.chars().count() as u32;
                    let lines = Point::new(0, display_text.len() as u32);
                    new_transforms.push(
                        Transform {
                            summary: TransformSummary {
                                display: TextSummary {
                                    bytes: display_text.len(),
                                    lines,
                                    first_line_chars: chars,
                                    last_line_chars: chars,
                                    rightmost_point: Point::new(0, chars),
                                },
                                buffer: buffer.text_summary_for_range(fold.start..fold.end),
                            },
                            display_text: Some(display_text),
                        },
                        &(),
                    );
                }
            }

            let sum = new_transforms.summary();
            if sum.buffer.bytes < edit.new_range.end {
                let text_summary =
                    buffer.text_summary_for_range(sum.buffer.bytes..edit.new_range.end);
                new_transforms.push(
                    Transform {
                        summary: TransformSummary {
                            display: text_summary.clone(),
                            buffer: text_summary,
                        },
                        display_text: None,
                    },
                    &(),
                );
            }
        }

        new_transforms.push_tree(cursor.suffix(&()), &());
        if new_transforms.is_empty() {
            let text_summary = buffer.text_summary();
            new_transforms.push(
                Transform {
                    summary: TransformSummary {
                        display: text_summary.clone(),
                        buffer: text_summary,
                    },
                    display_text: None,
                },
                &(),
            );
        }

        drop(cursor);
        *transforms = new_transforms;
    }
}

pub struct FoldMapSnapshot {
    transforms: SumTree<Transform>,
    buffer: ModelHandle<Buffer>,
}

impl FoldMapSnapshot {
    pub fn buffer_rows(&self, start_row: u32) -> BufferRows {
        if start_row > self.transforms.summary().display.lines.row {
            panic!("invalid display row {}", start_row);
        }

        let display_point = Point::new(start_row, 0);
        let mut cursor = self.transforms.cursor();
        cursor.seek(&DisplayPoint(display_point), SeekBias::Left, &());

        BufferRows {
            display_point,
            cursor,
        }
    }

    pub fn chunks_at<'a>(&'a self, offset: DisplayOffset, ctx: &'a AppContext) -> Chunks<'a> {
        let mut transform_cursor = self.transforms.cursor::<DisplayOffset, TransformSummary>();
        transform_cursor.seek(&offset, SeekBias::Right, &());
        let overshoot = offset.0 - transform_cursor.start().display.bytes;
        let buffer_offset = transform_cursor.start().buffer.bytes + overshoot;
        let buffer = self.buffer.read(ctx);
        let rope_cursor = buffer.text_for_range(buffer_offset..buffer.len());
        Chunks {
            transform_cursor,
            buffer_offset,
            buffer_chunks: rope_cursor,
        }
    }

    pub fn chars_at<'a>(
        &'a self,
        point: DisplayPoint,
        ctx: &'a AppContext,
    ) -> impl Iterator<Item = char> + 'a {
        let offset = self.to_display_offset(point, ctx);
        self.chunks_at(offset, ctx).flat_map(str::chars)
    }

    pub fn to_display_offset(&self, point: DisplayPoint, ctx: &AppContext) -> DisplayOffset {
        let mut cursor = self.transforms.cursor::<DisplayPoint, TransformSummary>();
        cursor.seek(&point, SeekBias::Right, &());
        let overshoot = point.0 - cursor.start().display.lines;
        let mut offset = cursor.start().display.bytes;
        if !overshoot.is_zero() {
            let transform = cursor.item().expect("display point out of range");
            assert!(transform.display_text.is_none());
            let end_buffer_offset =
                (cursor.start().buffer.lines + overshoot).to_offset(self.buffer.read(ctx));
            offset += end_buffer_offset - cursor.start().buffer.bytes;
        }
        DisplayOffset(offset)
    }

    pub fn to_buffer_offset(&self, point: DisplayPoint, ctx: &AppContext) -> usize {
        let mut cursor = self.transforms.cursor::<DisplayPoint, TransformSummary>();
        cursor.seek(&point, SeekBias::Right, &());
        let overshoot = point.0 - cursor.start().display.lines;
        (cursor.start().buffer.lines + overshoot).to_offset(self.buffer.read(ctx))
    }

    #[cfg(test)]
    pub fn clip_offset(
        &self,
        offset: DisplayOffset,
        bias: Bias,
        ctx: &AppContext,
    ) -> DisplayOffset {
        let mut cursor = self.transforms.cursor::<DisplayOffset, TransformSummary>();
        cursor.seek(&offset, SeekBias::Right, &());
        if let Some(transform) = cursor.item() {
            let transform_start = cursor.start().display.bytes;
            if transform.display_text.is_some() {
                if offset.0 == transform_start || matches!(bias, Bias::Left) {
                    DisplayOffset(transform_start)
                } else {
                    DisplayOffset(cursor.end().display.bytes)
                }
            } else {
                let overshoot = offset.0 - transform_start;
                let buffer_offset = cursor.start().buffer.bytes + overshoot;
                let clipped_buffer_offset = self.buffer.read(ctx).clip_offset(buffer_offset, bias);
                DisplayOffset(
                    (offset.0 as isize + (clipped_buffer_offset as isize - buffer_offset as isize))
                        as usize,
                )
            }
        } else {
            DisplayOffset(self.transforms.summary().display.bytes)
        }
    }

    pub fn clip_point(&self, point: DisplayPoint, bias: Bias, ctx: &AppContext) -> DisplayPoint {
        let mut cursor = self.transforms.cursor::<DisplayPoint, TransformSummary>();
        cursor.seek(&point, SeekBias::Right, &());
        if let Some(transform) = cursor.item() {
            let transform_start = cursor.start().display.lines;
            if transform.display_text.is_some() {
                if point.0 == transform_start || matches!(bias, Bias::Left) {
                    DisplayPoint(transform_start)
                } else {
                    DisplayPoint(cursor.end().display.lines)
                }
            } else {
                let overshoot = point.0 - transform_start;
                let buffer_position = cursor.start().buffer.lines + overshoot;
                let clipped_buffer_position =
                    self.buffer.read(ctx).clip_point(buffer_position, bias);
                DisplayPoint::new(
                    point.row(),
                    ((point.column() as i32) + clipped_buffer_position.column as i32
                        - buffer_position.column as i32) as u32,
                )
            }
        } else {
            DisplayPoint(self.transforms.summary().display.lines)
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct Transform {
    summary: TransformSummary,
    display_text: Option<&'static str>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct TransformSummary {
    display: TextSummary,
    buffer: TextSummary,
}

impl sum_tree::Item for Transform {
    type Summary = TransformSummary;

    fn summary(&self) -> Self::Summary {
        self.summary.clone()
    }
}

impl sum_tree::Summary for TransformSummary {
    type Context = ();

    fn add_summary(&mut self, other: &Self, _: &()) {
        self.buffer += &other.buffer;
        self.display += &other.display;
    }
}

impl<'a> sum_tree::Dimension<'a, TransformSummary> for TransformSummary {
    fn add_summary(&mut self, summary: &'a TransformSummary) {
        sum_tree::Summary::add_summary(self, summary, &());
    }
}

#[derive(Clone, Debug)]
struct Fold(Range<Anchor>);

impl Default for Fold {
    fn default() -> Self {
        Self(Anchor::Start..Anchor::End)
    }
}

impl sum_tree::Item for Fold {
    type Summary = FoldSummary;

    fn summary(&self) -> Self::Summary {
        FoldSummary {
            start: self.0.start.clone(),
            end: self.0.end.clone(),
            min_start: self.0.start.clone(),
            max_end: self.0.end.clone(),
            count: 1,
        }
    }
}

#[derive(Clone, Debug)]
struct FoldSummary {
    start: Anchor,
    end: Anchor,
    min_start: Anchor,
    max_end: Anchor,
    count: usize,
}

impl Default for FoldSummary {
    fn default() -> Self {
        Self {
            start: Anchor::Start,
            end: Anchor::End,
            min_start: Anchor::End,
            max_end: Anchor::Start,
            count: 0,
        }
    }
}

impl sum_tree::Summary for FoldSummary {
    type Context = Buffer;

    fn add_summary(&mut self, other: &Self, buffer: &Buffer) {
        if other.min_start.cmp(&self.min_start, buffer).unwrap() == Ordering::Less {
            self.min_start = other.min_start.clone();
        }
        if other.max_end.cmp(&self.max_end, buffer).unwrap() == Ordering::Greater {
            self.max_end = other.max_end.clone();
        }

        #[cfg(debug_assertions)]
        {
            let start_comparison = self.start.cmp(&other.start, buffer).unwrap();
            assert!(start_comparison <= Ordering::Equal);
            if start_comparison == Ordering::Equal {
                assert!(self.end.cmp(&other.end, buffer).unwrap() >= Ordering::Equal);
            }
        }
        self.start = other.start.clone();
        self.end = other.end.clone();
        self.count += other.count;
    }
}

impl<'a> sum_tree::Dimension<'a, FoldSummary> for Fold {
    fn add_summary(&mut self, summary: &'a FoldSummary) {
        self.0.start = summary.start.clone();
        self.0.end = summary.end.clone();
    }
}

impl<'a> sum_tree::SeekDimension<'a, FoldSummary> for Fold {
    fn cmp(&self, other: &Self, buffer: &Buffer) -> Ordering {
        self.0.cmp(&other.0, buffer).unwrap()
    }
}

impl<'a> sum_tree::Dimension<'a, FoldSummary> for usize {
    fn add_summary(&mut self, summary: &'a FoldSummary) {
        *self += summary.count;
    }
}

pub struct BufferRows<'a> {
    cursor: Cursor<'a, Transform, DisplayPoint, TransformSummary>,
    display_point: Point,
}

impl<'a> Iterator for BufferRows<'a> {
    type Item = u32;

    fn next(&mut self) -> Option<Self::Item> {
        while self.display_point > self.cursor.end().display.lines {
            self.cursor.next();
            if self.cursor.item().is_none() {
                // TODO: Return a bool from next?
                break;
            }
        }

        if self.cursor.item().is_some() {
            let overshoot = self.display_point - self.cursor.start().display.lines;
            let buffer_point = self.cursor.start().buffer.lines + overshoot;
            self.display_point.row += 1;
            Some(buffer_point.row)
        } else {
            None
        }
    }
}

pub struct Chunks<'a> {
    transform_cursor: Cursor<'a, Transform, DisplayOffset, TransformSummary>,
    buffer_chunks: buffer::ChunksIter<'a>,
    buffer_offset: usize,
}

impl<'a> Iterator for Chunks<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<Self::Item> {
        let transform = if let Some(item) = self.transform_cursor.item() {
            item
        } else {
            return None;
        };

        // If we're in a fold, then return the fold's display text and
        // advance the transform and buffer cursors to the end of the fold.
        if let Some(display_text) = transform.display_text {
            self.buffer_offset += transform.summary.buffer.bytes;
            self.buffer_chunks.advance_to(self.buffer_offset);

            while self.buffer_offset >= self.transform_cursor.end().buffer.bytes
                && self.transform_cursor.item().is_some()
            {
                self.transform_cursor.next();
            }

            return Some(display_text);
        }

        // Otherwise, take a chunk from the buffer's text.
        if let Some(mut chunk) = self.buffer_chunks.peek() {
            let offset_in_chunk = self.buffer_offset - self.buffer_chunks.offset();
            chunk = &chunk[offset_in_chunk..];

            // Truncate the chunk so that it ends at the next fold.
            let region_end = self.transform_cursor.end().buffer.bytes - self.buffer_offset;
            if chunk.len() >= region_end {
                chunk = &chunk[0..region_end];
                self.transform_cursor.next();
            } else {
                self.buffer_chunks.next();
            }

            self.buffer_offset += chunk.len();
            return Some(chunk);
        }

        None
    }
}

impl<'a> sum_tree::Dimension<'a, TransformSummary> for DisplayPoint {
    fn add_summary(&mut self, summary: &'a TransformSummary) {
        self.0 += &summary.display.lines;
    }
}

#[derive(Copy, Clone, Debug, Default, Eq, Ord, PartialOrd, PartialEq)]
pub struct DisplayOffset(usize);

impl<'a> sum_tree::Dimension<'a, TransformSummary> for DisplayOffset {
    fn add_summary(&mut self, summary: &'a TransformSummary) {
        self.0 += &summary.display.bytes;
    }
}

impl<'a> sum_tree::Dimension<'a, TransformSummary> for Point {
    fn add_summary(&mut self, summary: &'a TransformSummary) {
        *self += &summary.buffer.lines;
    }
}

impl<'a> sum_tree::Dimension<'a, TransformSummary> for usize {
    fn add_summary(&mut self, summary: &'a TransformSummary) {
        *self += &summary.buffer.bytes;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::buffer::ToPoint;
    use crate::test::sample_text;

    #[gpui::test]
    fn test_basic_folds(app: &mut gpui::MutableAppContext) {
        let buffer = app.add_model(|ctx| Buffer::new(0, sample_text(5, 6), ctx));
        let mut map = FoldMap::new(buffer.clone(), app.as_ref());

        map.fold(
            vec![
                Point::new(0, 2)..Point::new(2, 2),
                Point::new(2, 4)..Point::new(4, 1),
            ],
            app.as_ref(),
        );
        assert_eq!(map.text(app.as_ref()), "aa…cc…eeeee");

        buffer.update(app, |buffer, ctx| {
            buffer
                .edit(
                    vec![
                        Point::new(0, 0)..Point::new(0, 1),
                        Point::new(2, 3)..Point::new(2, 3),
                    ],
                    "123",
                    Some(ctx),
                )
                .unwrap();
        });
        assert_eq!(map.text(app.as_ref()), "123a…c123c…eeeee");

        buffer.update(app, |buffer, ctx| {
            let start_version = buffer.version.clone();
            buffer
                .edit(Some(Point::new(2, 6)..Point::new(4, 3)), "456", Some(ctx))
                .unwrap();
            buffer.edits_since(start_version).collect::<Vec<_>>()
        });
        assert_eq!(map.text(app.as_ref()), "123a…c123456eee");

        map.unfold(Some(Point::new(0, 4)..Point::new(0, 5)), app.as_ref());
        assert_eq!(map.text(app.as_ref()), "123aaaaa\nbbbbbb\nccc123456eee");
    }

    #[gpui::test]
    fn test_adjacent_folds(app: &mut gpui::MutableAppContext) {
        let buffer = app.add_model(|ctx| Buffer::new(0, "abcdefghijkl", ctx));

        {
            let mut map = FoldMap::new(buffer.clone(), app.as_ref());

            map.fold(vec![5..8], app.as_ref());
            map.check_invariants(app.as_ref());
            assert_eq!(map.text(app.as_ref()), "abcde…ijkl");

            // Create an fold adjacent to the start of the first fold.
            map.fold(vec![0..1, 2..5], app.as_ref());
            map.check_invariants(app.as_ref());
            assert_eq!(map.text(app.as_ref()), "…b…ijkl");

            // Create an fold adjacent to the end of the first fold.
            map.fold(vec![11..11, 8..10], app.as_ref());
            map.check_invariants(app.as_ref());
            assert_eq!(map.text(app.as_ref()), "…b…kl");
        }

        {
            let mut map = FoldMap::new(buffer.clone(), app.as_ref());

            // Create two adjacent folds.
            map.fold(vec![0..2, 2..5], app.as_ref());
            map.check_invariants(app.as_ref());
            assert_eq!(map.text(app.as_ref()), "…fghijkl");

            // Edit within one of the folds.
            buffer.update(app, |buffer, ctx| {
                let version = buffer.version();
                buffer.edit(vec![0..1], "12345", Some(ctx)).unwrap();
                buffer.edits_since(version).collect::<Vec<_>>()
            });
            map.check_invariants(app.as_ref());
            assert_eq!(map.text(app.as_ref()), "12345…fghijkl");
        }
    }

    #[gpui::test]
    fn test_overlapping_folds(app: &mut gpui::MutableAppContext) {
        let buffer = app.add_model(|ctx| Buffer::new(0, sample_text(5, 6), ctx));
        let mut map = FoldMap::new(buffer.clone(), app.as_ref());
        map.fold(
            vec![
                Point::new(0, 2)..Point::new(2, 2),
                Point::new(0, 4)..Point::new(1, 0),
                Point::new(1, 2)..Point::new(3, 2),
                Point::new(3, 1)..Point::new(4, 1),
            ],
            app.as_ref(),
        );
        assert_eq!(map.text(app.as_ref()), "aa…eeeee");
    }

    #[gpui::test]
    fn test_merging_folds_via_edit(app: &mut gpui::MutableAppContext) {
        let buffer = app.add_model(|ctx| Buffer::new(0, sample_text(5, 6), ctx));
        let mut map = FoldMap::new(buffer.clone(), app.as_ref());

        map.fold(
            vec![
                Point::new(0, 2)..Point::new(2, 2),
                Point::new(3, 1)..Point::new(4, 1),
            ],
            app.as_ref(),
        );
        assert_eq!(map.text(app.as_ref()), "aa…cccc\nd…eeeee");

        buffer.update(app, |buffer, ctx| {
            buffer
                .edit(Some(Point::new(2, 2)..Point::new(3, 1)), "", Some(ctx))
                .unwrap();
        });
        assert_eq!(map.text(app.as_ref()), "aa…eeeee");
    }

    #[gpui::test]
    fn test_folds_in_range(app: &mut gpui::MutableAppContext) {
        let buffer = app.add_model(|ctx| Buffer::new(0, sample_text(5, 6), ctx));
        let mut map = FoldMap::new(buffer.clone(), app.as_ref());
        let buffer = buffer.read(app);

        map.fold(
            vec![
                Point::new(0, 2)..Point::new(2, 2),
                Point::new(0, 4)..Point::new(1, 0),
                Point::new(1, 2)..Point::new(3, 2),
                Point::new(3, 1)..Point::new(4, 1),
            ],
            app.as_ref(),
        );
        let fold_ranges = map
            .folds_in_range(Point::new(1, 0)..Point::new(1, 3), app.as_ref())
            .map(|fold| fold.start.to_point(buffer)..fold.end.to_point(buffer))
            .collect::<Vec<_>>();
        assert_eq!(
            fold_ranges,
            vec![
                Point::new(0, 2)..Point::new(2, 2),
                Point::new(1, 2)..Point::new(3, 2)
            ]
        );
    }

    #[gpui::test]
    fn test_random_folds(app: &mut gpui::MutableAppContext) {
        use crate::editor::ToPoint;
        use crate::util::RandomCharIter;
        use rand::prelude::*;
        use std::env;
        use Bias::{Left, Right};

        let iterations = env::var("ITERATIONS")
            .map(|i| i.parse().expect("invalid `ITERATIONS` variable"))
            .unwrap_or(100);
        let operations = env::var("OPERATIONS")
            .map(|i| i.parse().expect("invalid `OPERATIONS` variable"))
            .unwrap_or(10);
        let seed_range = if let Ok(seed) = env::var("SEED") {
            let seed = seed.parse().expect("invalid `SEED` variable");
            seed..seed + 1
        } else {
            0..iterations
        };

        for seed in seed_range {
            dbg!(seed);
            let mut rng = StdRng::seed_from_u64(seed);

            let buffer = app.add_model(|ctx| {
                let len = rng.gen_range(0..10);
                let text = RandomCharIter::new(&mut rng).take(len).collect::<String>();
                Buffer::new(0, text, ctx)
            });
            let mut map = FoldMap::new(buffer.clone(), app.as_ref());

            for _ in 0..operations {
                log::info!("text: {:?}", buffer.read(app).text());
                match rng.gen_range(0..=100) {
                    0..=34 => {
                        let buffer = buffer.read(app);
                        let mut to_fold = Vec::new();
                        for _ in 0..rng.gen_range(1..=5) {
                            let end = buffer.clip_offset(rng.gen_range(0..=buffer.len()), Right);
                            let start = buffer.clip_offset(rng.gen_range(0..=end), Left);
                            to_fold.push(start..end);
                        }
                        log::info!("folding {:?}", to_fold);
                        map.fold(to_fold, app.as_ref());
                    }
                    35..=59 if !map.folds.is_empty() => {
                        let buffer = buffer.read(app);
                        let mut to_unfold = Vec::new();
                        for _ in 0..rng.gen_range(1..=3) {
                            let end = buffer.clip_offset(rng.gen_range(0..=buffer.len()), Right);
                            let start = buffer.clip_offset(rng.gen_range(0..=end), Left);
                            to_unfold.push(start..end);
                        }
                        log::info!("unfolding {:?}", to_unfold);
                        map.unfold(to_unfold, app.as_ref());
                    }
                    _ => {
                        let edits = buffer.update(app, |buffer, ctx| {
                            let start_version = buffer.version.clone();
                            let edit_count = rng.gen_range(1..=5);
                            buffer.randomly_edit(&mut rng, edit_count, Some(ctx));
                            buffer.edits_since(start_version).collect::<Vec<_>>()
                        });
                        log::info!("editing {:?}", edits);
                    }
                }
                map.check_invariants(app.as_ref());

                let buffer = map.buffer.read(app);
                let mut expected_text = buffer.text();
                let mut expected_buffer_rows = Vec::new();
                let mut next_row = buffer.max_point().row;
                for fold_range in map.merged_fold_ranges(app.as_ref()).into_iter().rev() {
                    let fold_start = buffer.point_for_offset(fold_range.start).unwrap();
                    let fold_end = buffer.point_for_offset(fold_range.end).unwrap();
                    expected_buffer_rows.extend((fold_end.row + 1..=next_row).rev());
                    next_row = fold_start.row;

                    expected_text.replace_range(fold_range.start..fold_range.end, "…");
                }
                expected_buffer_rows.extend((0..=next_row).rev());
                expected_buffer_rows.reverse();

                assert_eq!(map.text(app.as_ref()), expected_text);

                for (display_row, line) in expected_text.lines().enumerate() {
                    let line_len = map.line_len(display_row as u32, app.as_ref());
                    assert_eq!(line_len, line.len() as u32);
                }

                let rightmost_row = map.rightmost_row(app.as_ref());
                let rightmost_char_column = expected_text
                    .split('\n')
                    .nth(rightmost_row as usize)
                    .unwrap()
                    .chars()
                    .count();
                let mut display_point = DisplayPoint::new(0, 0);
                let mut display_offset = DisplayOffset(0);
                let mut char_column = 0;
                for c in expected_text.chars() {
                    let buffer_point = map.to_buffer_point(display_point, app.as_ref());
                    let buffer_offset = buffer_point.to_offset(buffer);
                    assert_eq!(
                        map.to_display_point(buffer_point, app.as_ref()),
                        display_point,
                        "to_display_point({:?})",
                        buffer_point,
                    );
                    assert_eq!(
                        map.to_buffer_offset(display_point, app.as_ref()),
                        buffer_offset,
                        "to_buffer_offset({:?})",
                        display_point,
                    );
                    assert_eq!(
                        map.to_display_offset(display_point, app.as_ref()),
                        display_offset,
                        "to_display_offset({:?})",
                        display_point,
                    );

                    if c == '\n' {
                        *display_point.row_mut() += 1;
                        *display_point.column_mut() = 0;
                        char_column = 0;
                    } else {
                        *display_point.column_mut() += c.len_utf8() as u32;
                        char_column += 1;
                    }
                    display_offset.0 += c.len_utf8();
                    if char_column > rightmost_char_column {
                        panic!(
                            "invalid rightmost row {:?} (chars {}), found row {:?} (chars: {})",
                            rightmost_row,
                            rightmost_char_column,
                            display_point.row(),
                            char_column
                        );
                    }
                }

                for _ in 0..5 {
                    let offset = map.snapshot(app.as_ref()).clip_offset(
                        DisplayOffset(rng.gen_range(0..=map.len(app.as_ref()))),
                        Bias::Right,
                        app.as_ref(),
                    );
                    assert_eq!(
                        map.snapshot(app.as_ref())
                            .chunks_at(offset, app.as_ref())
                            .collect::<String>(),
                        &expected_text[offset.0..],
                    );
                }

                for (idx, buffer_row) in expected_buffer_rows.iter().enumerate() {
                    let display_row = map
                        .to_display_point(Point::new(*buffer_row, 0), app.as_ref())
                        .row();
                    assert_eq!(
                        map.snapshot(app.as_ref())
                            .buffer_rows(display_row)
                            .collect::<Vec<_>>(),
                        expected_buffer_rows[idx..],
                    );
                }

                for fold_range in map.merged_fold_ranges(app.as_ref()) {
                    let display_point =
                        map.to_display_point(fold_range.start.to_point(buffer), app.as_ref());
                    assert!(map.is_line_folded(display_point.row(), app.as_ref()));
                }

                for _ in 0..5 {
                    let end = buffer.clip_offset(rng.gen_range(0..=buffer.len()), Right);
                    let start = buffer.clip_offset(rng.gen_range(0..=end), Left);
                    let expected_folds = map
                        .folds
                        .items()
                        .into_iter()
                        .filter(|fold| {
                            let start = buffer.anchor_before(start);
                            let end = buffer.anchor_after(end);
                            start.cmp(&fold.0.end, buffer).unwrap() == Ordering::Less
                                && end.cmp(&fold.0.start, buffer).unwrap() == Ordering::Greater
                        })
                        .map(|fold| fold.0)
                        .collect::<Vec<_>>();

                    assert_eq!(
                        map.folds_in_range(start..end, app.as_ref())
                            .cloned()
                            .collect::<Vec<_>>(),
                        expected_folds
                    );
                }
            }
        }
    }

    #[gpui::test]
    fn test_buffer_rows(app: &mut gpui::MutableAppContext) {
        let text = sample_text(6, 6) + "\n";
        let buffer = app.add_model(|ctx| Buffer::new(0, text, ctx));

        let mut map = FoldMap::new(buffer.clone(), app.as_ref());

        map.fold(
            vec![
                Point::new(0, 2)..Point::new(2, 2),
                Point::new(3, 1)..Point::new(4, 1),
            ],
            app.as_ref(),
        );

        assert_eq!(map.text(app.as_ref()), "aa…cccc\nd…eeeee\nffffff\n");
        assert_eq!(
            map.snapshot(app.as_ref())
                .buffer_rows(0)
                .collect::<Vec<_>>(),
            vec![0, 3, 5, 6]
        );
        assert_eq!(
            map.snapshot(app.as_ref())
                .buffer_rows(3)
                .collect::<Vec<_>>(),
            vec![6]
        );
    }

    impl FoldMap {
        fn text(&self, app: &AppContext) -> String {
            self.snapshot(app)
                .chunks_at(DisplayOffset(0), app)
                .collect()
        }

        fn merged_fold_ranges(&self, app: &AppContext) -> Vec<Range<usize>> {
            let buffer = self.buffer.read(app);
            let mut folds = self.folds.items();
            // Ensure sorting doesn't change how folds get merged and displayed.
            folds.sort_by(|a, b| a.0.cmp(&b.0, buffer).unwrap());
            let mut fold_ranges = folds
                .iter()
                .map(|fold| fold.0.start.to_offset(buffer)..fold.0.end.to_offset(buffer))
                .peekable();

            let mut merged_ranges = Vec::new();
            while let Some(mut fold_range) = fold_ranges.next() {
                while let Some(next_range) = fold_ranges.peek() {
                    if fold_range.end >= next_range.start {
                        if next_range.end > fold_range.end {
                            fold_range.end = next_range.end;
                        }
                        fold_ranges.next();
                    } else {
                        break;
                    }
                }
                if fold_range.end > fold_range.start {
                    merged_ranges.push(fold_range);
                }
            }
            merged_ranges
        }

        fn check_invariants(&self, ctx: &AppContext) {
            let transforms = self.sync(ctx);
            let buffer = self.buffer.read(ctx);
            assert_eq!(
                transforms.summary().buffer.bytes,
                buffer.len(),
                "transform tree does not match buffer's length"
            );
        }
    }
}
