// SPDX-License-Identifier: MIT OR Apache-2.0

#[cfg(not(feature = "std"))]
use alloc::vec::Vec;
use core::cmp::{max, min};
use core::mem;
use core::ops::Range;
use unicode_script::{Script, UnicodeScript};
use unicode_segmentation::UnicodeSegmentation;

use crate::fallback::FontFallbackIter;
use crate::{Align, AttrsList, CacheKey, Color, Font, FontSystem, LayoutGlyph, LayoutLine, Wrap};

fn shape_fallback(
    font: &Font,
    line: &str,
    attrs_list: &AttrsList,
    start_run: usize,
    end_run: usize,
    span_rtl: bool,
) -> (Vec<ShapeGlyph>, Vec<usize>) {
    let run = &line[start_run..end_run];

    let font_scale = font.rustybuzz.units_per_em() as f32;

    let mut buffer = rustybuzz::UnicodeBuffer::new();
    buffer.set_direction(if span_rtl {
        rustybuzz::Direction::RightToLeft
    } else {
        rustybuzz::Direction::LeftToRight
    });
    buffer.push_str(run);
    buffer.guess_segment_properties();

    let rtl = matches!(buffer.direction(), rustybuzz::Direction::RightToLeft);
    assert_eq!(rtl, span_rtl);

    let glyph_buffer = rustybuzz::shape(&font.rustybuzz, &[], buffer);
    let glyph_infos = glyph_buffer.glyph_infos();
    let glyph_positions = glyph_buffer.glyph_positions();

    let mut missing = Vec::new();
    let mut glyphs = Vec::with_capacity(glyph_infos.len());
    for (info, pos) in glyph_infos.iter().zip(glyph_positions.iter()) {
        let x_advance = pos.x_advance as f32 / font_scale;
        let y_advance = pos.y_advance as f32 / font_scale;
        let x_offset = pos.x_offset as f32 / font_scale;
        let y_offset = pos.y_offset as f32 / font_scale;

        let start_glyph = start_run + info.cluster as usize;

        //println!("  {:?} {:?}", info, pos);
        if info.glyph_id == 0 {
            missing.push(start_glyph);
        }

        let attrs = attrs_list.get_span(start_glyph);
        glyphs.push(ShapeGlyph {
            start: start_glyph,
            end: end_run, // Set later
            x_advance,
            y_advance,
            x_offset,
            y_offset,
            font_id: font.info.id,
            glyph_id: info.glyph_id.try_into().expect("failed to cast glyph ID"),
            //TODO: color should not be related to shaping
            color_opt: attrs.color_opt,
            metadata: attrs.metadata,
        });
    }

    // Adjust end of glyphs
    if rtl {
        for i in 1..glyphs.len() {
            let next_start = glyphs[i - 1].start;
            let next_end = glyphs[i - 1].end;
            let prev = &mut glyphs[i];
            if prev.start == next_start {
                prev.end = next_end;
            } else {
                prev.end = next_start;
            }
        }
    } else {
        for i in (1..glyphs.len()).rev() {
            let next_start = glyphs[i].start;
            let next_end = glyphs[i].end;
            let prev = &mut glyphs[i - 1];
            if prev.start == next_start {
                prev.end = next_end;
            } else {
                prev.end = next_start;
            }
        }
    }

    (glyphs, missing)
}

fn shape_run<'a>(
    font_system: &'a FontSystem,
    line: &str,
    attrs_list: &AttrsList,
    start_run: usize,
    end_run: usize,
    span_rtl: bool,
) -> Vec<ShapeGlyph> {
    //TODO: use smallvec?
    let mut scripts = Vec::new();
    for c in line[start_run..end_run].chars() {
        match c.script() {
            Script::Common | Script::Inherited | Script::Latin | Script::Unknown => (),
            script => {
                if !scripts.contains(&script) {
                    scripts.push(script);
                }
            }
        }
    }

    log::trace!("      Run {:?}: '{}'", scripts, &line[start_run..end_run],);

    let attrs = attrs_list.get_span(start_run);

    let font_matches = font_system.get_font_matches(attrs);

    let default_families = [font_matches.default_family.as_str()];
    let mut font_iter = FontFallbackIter::new(
        &font_matches.fonts,
        &default_families,
        scripts,
        font_matches.locale,
    );

    let (mut glyphs, mut missing) = shape_fallback(
        font_iter.next().expect("no default font found"),
        line,
        attrs_list,
        start_run,
        end_run,
        span_rtl,
    );

    //TODO: improve performance!
    while !missing.is_empty() {
        let font = match font_iter.next() {
            Some(some) => some,
            None => break,
        };

        log::trace!("Evaluating fallback with font '{}'", font.info.family);
        let (mut fb_glyphs, fb_missing) =
            shape_fallback(font, line, attrs_list, start_run, end_run, span_rtl);

        // Insert all matching glyphs
        let mut fb_i = 0;
        while fb_i < fb_glyphs.len() {
            let start = fb_glyphs[fb_i].start;
            let end = fb_glyphs[fb_i].end;

            // Skip clusters that are not missing, or where the fallback font is missing
            if !missing.contains(&start) || fb_missing.contains(&start) {
                fb_i += 1;
                continue;
            }

            let mut missing_i = 0;
            while missing_i < missing.len() {
                if missing[missing_i] >= start && missing[missing_i] < end {
                    // println!("No longer missing {}", missing[missing_i]);
                    missing.remove(missing_i);
                } else {
                    missing_i += 1;
                }
            }

            // Find prior glyphs
            let mut i = 0;
            while i < glyphs.len() {
                if glyphs[i].start >= start && glyphs[i].end <= end {
                    break;
                } else {
                    i += 1;
                }
            }

            // Remove prior glyphs
            while i < glyphs.len() {
                if glyphs[i].start >= start && glyphs[i].end <= end {
                    let _glyph = glyphs.remove(i);
                    // log::trace!("Removed {},{} from {}", _glyph.start, _glyph.end, i);
                } else {
                    break;
                }
            }

            while fb_i < fb_glyphs.len() {
                if fb_glyphs[fb_i].start >= start && fb_glyphs[fb_i].end <= end {
                    let fb_glyph = fb_glyphs.remove(fb_i);
                    // log::trace!("Insert {},{} from font {} at {}", fb_glyph.start, fb_glyph.end, font_i, i);
                    glyphs.insert(i, fb_glyph);
                    i += 1;
                } else {
                    break;
                }
            }
        }
    }

    // Debug missing font fallbacks
    font_iter.check_missing(&line[start_run..end_run]);

    /*
    for glyph in glyphs.iter() {
        log::trace!("'{}': {}, {}, {}, {}", &line[glyph.start..glyph.end], glyph.x_advance, glyph.y_advance, glyph.x_offset, glyph.y_offset);
    }
    */

    glyphs
}

/// A shaped glyph
pub struct ShapeGlyph {
    pub start: usize,
    pub end: usize,
    pub x_advance: f32,
    pub y_advance: f32,
    pub x_offset: f32,
    pub y_offset: f32,
    pub font_id: fontdb::ID,
    pub glyph_id: u16,
    pub color_opt: Option<Color>,
    pub metadata: usize,
}

impl ShapeGlyph {
    fn layout(&self, font_size: i32, x: f32, y: f32, level: unicode_bidi::Level) -> LayoutGlyph {
        let x_offset = font_size as f32 * self.x_offset;
        let y_offset = font_size as f32 * self.y_offset;
        let x_advance = font_size as f32 * self.x_advance;

        let (cache_key, x_int, y_int) = CacheKey::new(
            self.font_id,
            self.glyph_id,
            font_size,
            (x + x_offset, y - y_offset),
        );
        LayoutGlyph {
            start: self.start,
            end: self.end,
            x,
            w: x_advance,
            level,
            cache_key,
            x_offset,
            y_offset,
            x_int,
            y_int,
            color_opt: self.color_opt,
            metadata: self.metadata,
        }
    }
}

/// A shaped word (for word wrapping)
pub struct ShapeWord {
    pub blank: bool,
    pub glyphs: Vec<ShapeGlyph>,
    pub x_advance: f32,
    pub y_advance: f32,
}

impl ShapeWord {
    pub fn new<'a>(
        font_system: &'a FontSystem,
        line: &str,
        attrs_list: &AttrsList,
        word_range: Range<usize>,
        level: unicode_bidi::Level,
        blank: bool,
    ) -> Self {
        let word = &line[word_range.clone()];

        log::trace!(
            "      Word{}: '{}'",
            if blank { " BLANK" } else { "" },
            word
        );

        let mut glyphs = Vec::new();
        let span_rtl = level.is_rtl();

        let mut start_run = word_range.start;
        let mut attrs = attrs_list.defaults();
        for (egc_i, _egc) in word.grapheme_indices(true) {
            let start_egc = word_range.start + egc_i;
            let attrs_egc = attrs_list.get_span(start_egc);
            if !attrs.compatible(&attrs_egc) {
                //TODO: more efficient
                glyphs.append(&mut shape_run(
                    font_system,
                    line,
                    attrs_list,
                    start_run,
                    start_egc,
                    span_rtl,
                ));

                start_run = start_egc;
                attrs = attrs_egc;
            }
        }
        if start_run < word_range.end {
            //TODO: more efficient
            glyphs.append(&mut shape_run(
                font_system,
                line,
                attrs_list,
                start_run,
                word_range.end,
                span_rtl,
            ));
        }

        let mut x_advance = 0.0;
        let mut y_advance = 0.0;
        for glyph in &glyphs {
            x_advance += glyph.x_advance;
            y_advance += glyph.y_advance;
        }

        Self {
            blank,
            glyphs,
            x_advance,
            y_advance,
        }
    }
}

/// A shaped span (for bidirectional processing)
pub struct ShapeSpan {
    pub level: unicode_bidi::Level,
    pub words: Vec<ShapeWord>,
}

impl ShapeSpan {
    pub fn new<'a>(
        font_system: &'a FontSystem,
        line: &str,
        attrs_list: &AttrsList,
        span_range: Range<usize>,
        line_rtl: bool,
        level: unicode_bidi::Level,
    ) -> Self {
        let span = &line[span_range.start..span_range.end];

        log::trace!(
            "  Span {}: '{}'",
            if level.is_rtl() { "RTL" } else { "LTR" },
            span
        );

        let mut words = Vec::new();

        let mut start_word = 0;
        for (end_lb, _) in unicode_linebreak::linebreaks(span) {
            let mut start_lb = end_lb;
            for (i, c) in span[start_word..end_lb].char_indices() {
                if start_word + i == end_lb {
                    break;
                } else if c.is_whitespace() {
                    start_lb = start_word + i;
                    break;
                }
            }
            if start_word < start_lb {
                words.push(ShapeWord::new(
                    font_system,
                    line,
                    attrs_list,
                    (span_range.start + start_word)..(span_range.start + start_lb),
                    level,
                    false,
                ));
            }
            if start_lb < end_lb {
                for (i, c) in span[start_lb..end_lb].char_indices() {
                    // assert!(c.is_whitespace());
                    words.push(ShapeWord::new(
                        font_system,
                        line,
                        attrs_list,
                        (span_range.start + start_lb + i)
                            ..(span_range.start + start_lb + i + c.len_utf8()),
                        level,
                        true,
                    ));
                }
            }
            start_word = end_lb;
        }

        // Reverse glyphs in RTL lines
        if line_rtl {
            for word in &mut words {
                word.glyphs.reverse();
            }
        }

        // Reverse words in spans that do not match line direction
        if line_rtl != level.is_rtl() {
            words.reverse();
        }

        ShapeSpan { level, words }
    }
}

/// A shaped line (or paragraph)
pub struct ShapeLine {
    pub rtl: bool,
    pub spans: Vec<ShapeSpan>,
}

// Visual Line Ranges: (span_index, (first_word_index, first_glyph_index), (last_word_index, last_glyph_index))
type VlRange = (usize, (usize, usize), (usize, usize));

impl ShapeLine {
    pub fn new<'a>(font_system: &'a FontSystem, line: &str, attrs_list: &AttrsList) -> Self {
        let mut spans = Vec::new();

        let bidi = unicode_bidi::BidiInfo::new(line, None);
        let rtl = if bidi.paragraphs.is_empty() {
            false
        } else {
            assert_eq!(bidi.paragraphs.len(), 1);
            let para_info = &bidi.paragraphs[0];
            let line_rtl = para_info.level.is_rtl();

            log::trace!("Line {}: '{}'", if line_rtl { "RTL" } else { "LTR" }, line);

            let line_range = para_info.range.clone();
            let levels = Self::adjust_levels(&unicode_bidi::Paragraph::new(&bidi, para_info));

            // Find consecutive level runs. We use this to create Spans.
            // Each span is a set of characters with equal levels.
            let mut start = line_range.start;
            let mut run_level = levels[start];

            for (i, &new_level) in levels
                .iter()
                .enumerate()
                .take(line_range.end)
                .skip(start + 1)
            {
                if new_level != run_level {
                    // End of the previous run, start of a new one.
                    spans.push(ShapeSpan::new(
                        font_system,
                        line,
                        attrs_list,
                        start..i,
                        line_rtl,
                        run_level,
                    ));
                    start = i;
                    run_level = new_level;
                }
            }
            spans.push(ShapeSpan::new(
                font_system,
                line,
                attrs_list,
                start..line_range.end,
                line_rtl,
                run_level,
            ));
            line_rtl
        };

        Self { rtl, spans }
    }

    // A modified version of first part of unicode_bidi::bidi_info::visual_run
    fn adjust_levels(para: &unicode_bidi::Paragraph) -> Vec<unicode_bidi::Level> {
        use unicode_bidi::BidiClass::*;
        let text = para.info.text;
        let levels = &para.info.levels;
        let original_classes = &para.info.original_classes;

        let mut levels = levels.clone();
        let line_classes = &original_classes[..];
        let line_levels = &mut levels[..];

        // Reset some whitespace chars to paragraph level.
        // <http://www.unicode.org/reports/tr9/#L1>
        let mut reset_from: Option<usize> = Some(0);
        let mut reset_to: Option<usize> = None;
        for (i, c) in text.char_indices() {
            match line_classes[i] {
                // Ignored by X9
                RLE | LRE | RLO | LRO | PDF | BN => {}
                // Segment separator, Paragraph separator
                B | S => {
                    assert_eq!(reset_to, None);
                    reset_to = Some(i + c.len_utf8());
                    if reset_from.is_none() {
                        reset_from = Some(i);
                    }
                }
                // Whitespace, isolate formatting
                WS | FSI | LRI | RLI | PDI => {
                    if reset_from.is_none() {
                        reset_from = Some(i);
                    }
                }
                _ => {
                    reset_from = None;
                }
            }
            if let (Some(from), Some(to)) = (reset_from, reset_to) {
                for level in &mut line_levels[from..to] {
                    *level = para.para.level;
                }
                reset_from = None;
                reset_to = None;
            }
        }
        if let Some(from) = reset_from {
            for level in &mut line_levels[from..] {
                *level = para.para.level;
            }
        }
        levels
    }

    // A modified version of second part of unicode_bidi::bidi_info::visual run
    fn reorder(&self, line_range: &[VlRange]) -> Vec<Range<usize>> {
        let line: Vec<unicode_bidi::Level> = line_range
            .iter()
            .map(|(span_index, _, _)| self.spans[*span_index].level)
            .collect();
        // Find consecutive level runs.
        let mut runs = Vec::new();
        let mut start = 0;
        let mut run_level = line[start];
        let mut min_level = run_level;
        let mut max_level = run_level;

        for (i, &new_level) in line.iter().enumerate().skip(start + 1) {
            if new_level != run_level {
                // End of the previous run, start of a new one.
                runs.push(start..i);
                start = i;
                run_level = new_level;
                min_level = min(run_level, min_level);
                max_level = max(run_level, max_level);
            }
        }
        runs.push(start..line.len());

        let run_count = runs.len();

        // Re-order the odd runs.
        // <http://www.unicode.org/reports/tr9/#L2>

        // Stop at the lowest *odd* level.
        min_level = min_level.new_lowest_ge_rtl().expect("Level error");

        while max_level >= min_level {
            // Look for the start of a sequence of consecutive runs of max_level or higher.
            let mut seq_start = 0;
            while seq_start < run_count {
                if line[runs[seq_start].start] < max_level {
                    seq_start += 1;
                    continue;
                }

                // Found the start of a sequence. Now find the end.
                let mut seq_end = seq_start + 1;
                while seq_end < run_count {
                    if line[runs[seq_end].start] < max_level {
                        break;
                    }
                    seq_end += 1;
                }

                // Reverse the runs within this sequence.
                runs[seq_start..seq_end].reverse();

                seq_start = seq_end;
            }
            max_level
                .lower(1)
                .expect("Lowering embedding level below zero");
        }

        runs
    }

    pub fn layout(
        &self,
        font_size: i32,
        line_width: i32,
        wrap: Wrap,
        align: Align,
    ) -> Vec<LayoutLine> {
        let mut layout_lines = Vec::with_capacity(1);

        // This is used to create a visual line for empty lines (e.g. lines with only a <CR>)
        let mut push_line = true;

        #[derive(Default)]
        struct VisualLine {
            ranges: Vec<VlRange>,
            spaces: u32,
            w: f32,
        }
        // For each visual line a list of  (span index,  and range of words in that span)
        // Note that a BiDi visual line could have multiple spans or parts of them
        // let mut vl_range_of_spans = Vec::with_capacity(1);
        let mut vl_range_of_spans: Vec<VisualLine> = Vec::with_capacity(1);

        let start_x = if self.rtl { line_width as f32 } else { 0.0 };
        let end_x = if self.rtl { 0.0 } else { line_width as f32 };
        let mut x = start_x;
        let mut y;

        // This would keep the maximum number of spans that would fit on a visual line
        // If one span is too large, this variable will hold the range of words inside that span
        // that fits on a line.
        // let mut current_visual_line: Vec<VlRange> = Vec::with_capacity(1);
        let mut current_visual_line = VisualLine::default();

        if wrap == Wrap::None {
            for (span_index, span) in self.spans.iter().enumerate() {
                current_visual_line
                    .ranges
                    .push((span_index, (0, 0), (span.words.len(), 0)));
            }
        } else {
            let mut fit_x = line_width as f32;
            for (span_index, span) in self.spans.iter().enumerate() {
                let mut word_ranges = Vec::new();
                let mut word_range_width = 0.;
                let mut number_of_blanks = 0;

                // Create the word ranges that fits in a visual line
                if self.rtl != span.level.is_rtl() {
                    // incongruent directions
                    let mut fitting_start = (span.words.len(), 0);
                    for (i, word) in span.words.iter().enumerate().rev() {
                        let word_size = font_size as f32 * word.x_advance;
                        if fit_x - word_size >= 0. {
                            // fits
                            fit_x -= word_size;
                            word_range_width += word_size;
                            if word.blank {
                                number_of_blanks += 1;
                            }
                            continue;
                        } else if wrap == Wrap::Glyph {
                            for (glyph_i, glyph) in word.glyphs.iter().enumerate().rev() {
                                let glyph_size = font_size as f32 * glyph.x_advance;
                                if fit_x - glyph_size >= 0. {
                                    fit_x -= glyph_size;
                                    word_range_width += glyph_size;
                                    continue;
                                } else {
                                    word_ranges.push((
                                        (i, glyph_i + 1),
                                        fitting_start,
                                        word_range_width,
                                        number_of_blanks,
                                    ));
                                    number_of_blanks = 0;
                                    fit_x = line_width as f32 - glyph_size;
                                    word_range_width = glyph_size;
                                    fitting_start = (i, glyph_i + 1);
                                }
                            }
                        } else {
                            // Wrap::Word
                            let mut prev_word_width = None;
                            if word.blank && number_of_blanks > 0 {
                                // current word causing a wrap is a space so we ignore it
                                number_of_blanks -= 1;
                            } else if let Some(previous_word) = span.words.get(i - 1) {
                                // Current word causing a wrap is not whitespace, so we ignore the
                                // previous word if it's a whitespace
                                if previous_word.blank {
                                    number_of_blanks -= 1;
                                    prev_word_width =
                                        Some(previous_word.x_advance * font_size as f32)
                                }
                            }
                            if let Some(width) = prev_word_width {
                                word_ranges.push((
                                    (i, 0),
                                    fitting_start,
                                    word_range_width - width,
                                    number_of_blanks,
                                ));
                            } else {
                                word_ranges.push((
                                    (i + 1, 0),
                                    fitting_start,
                                    word_range_width,
                                    number_of_blanks,
                                ));
                            }
                            number_of_blanks = 0;
                            if word.blank {
                                fit_x = line_width as f32;
                                word_range_width = 0.;
                                fitting_start = (i + 1, 0);
                            } else {
                                fit_x = line_width as f32 - word_size;
                                word_range_width = word_size;
                                fitting_start = (i + 1, 0);
                            }
                        }
                    }
                    word_ranges.push(((0, 0), fitting_start, word_range_width, number_of_blanks));
                } else {
                    // congruent direction
                    let mut fitting_start = (0, 0);
                    for (i, word) in span.words.iter().enumerate() {
                        let word_size = font_size as f32 * word.x_advance;
                        if fit_x - word_size >= 0. {
                            // fits
                            fit_x -= word_size;
                            word_range_width += word_size;
                            if word.blank {
                                number_of_blanks += 1;
                            }
                            continue;
                        } else if wrap == Wrap::Glyph {
                            for (glyph_i, glyph) in word.glyphs.iter().enumerate() {
                                let glyph_size = font_size as f32 * glyph.x_advance;
                                if fit_x - glyph_size >= 0. {
                                    fit_x -= glyph_size;
                                    word_range_width += glyph_size;
                                    continue;
                                } else {
                                    word_ranges.push((
                                        fitting_start,
                                        (i, glyph_i),
                                        word_range_width,
                                        number_of_blanks,
                                    ));
                                    number_of_blanks = 0;
                                    fit_x = line_width as f32 - glyph_size;
                                    word_range_width = glyph_size;
                                    fitting_start = (i, glyph_i);
                                }
                            }
                        } else {
                            // Wrap::Word
                            let mut prev_word_width = None;
                            if word.blank && number_of_blanks > 0 {
                                // current word causing a wrap is a space so we ignore it
                                number_of_blanks -= 1;
                            } else if let Some(previous_word) = span.words.get(i - 1) {
                                // Current word causing a wrap is not whitespace, so we ignore the
                                // previous word if it's a whitespace
                                if previous_word.blank {
                                    number_of_blanks -= 1;
                                    prev_word_width =
                                        Some(previous_word.x_advance * font_size as f32)
                                }
                            }
                            if let Some(width) = prev_word_width {
                                word_ranges.push((
                                    fitting_start,
                                    (i - 1, 0),
                                    word_range_width - width,
                                    number_of_blanks,
                                ));
                            } else {
                                word_ranges.push((
                                    fitting_start,
                                    (i, 0),
                                    word_range_width,
                                    number_of_blanks,
                                ));
                            }
                            number_of_blanks = 0;

                            if word.blank {
                                fit_x = line_width as f32;
                                word_range_width = 0.;
                                fitting_start = (i + 1, 0);
                            } else {
                                fit_x = line_width as f32 - word_size;
                                word_range_width = word_size;
                                fitting_start = (i, 0);
                            }
                        }
                    }
                    word_ranges.push((
                        fitting_start,
                        (span.words.len(), 0),
                        word_range_width,
                        number_of_blanks,
                    ));
                }

                // Create a visual line
                for (
                    (starting_word, starting_glyph),
                    (ending_word, ending_glyph),
                    word_range_width,
                    number_of_blanks,
                ) in word_ranges
                {
                    // To simplify the algorithm above, we might push empty ranges but we ignore them here
                    if ending_word == starting_word && starting_glyph == ending_glyph {
                        continue;
                    }

                    let fits = !if self.rtl {
                        x - word_range_width < end_x
                    } else {
                        x + word_range_width > end_x
                    };

                    if fits {
                        current_visual_line.ranges.push((
                            span_index,
                            (starting_word, starting_glyph),
                            (ending_word, ending_glyph),
                        ));
                        current_visual_line.w += word_range_width;
                        current_visual_line.spaces += number_of_blanks;
                        if self.rtl {
                            x -= word_range_width;
                        } else {
                            x += word_range_width;
                        }
                    } else {
                        if !current_visual_line.ranges.is_empty() {
                            vl_range_of_spans.push(current_visual_line);
                            current_visual_line = VisualLine::default();
                            x = start_x;
                        }
                        current_visual_line.ranges.push((
                            span_index,
                            (starting_word, starting_glyph),
                            (ending_word, ending_glyph),
                        ));
                        current_visual_line.w += word_range_width;
                        current_visual_line.spaces += number_of_blanks;
                        if self.rtl {
                            x -= word_range_width;
                        } else {
                            x += word_range_width;
                        }
                        if word_range_width > line_width as f32 {
                            // single word is bigger than line_width
                            vl_range_of_spans.push(current_visual_line);
                            current_visual_line = VisualLine::default();
                            x = start_x;
                        }
                    }
                }
            }
        }

        if !current_visual_line.ranges.is_empty() {
            vl_range_of_spans.push(current_visual_line);
        }

        // Create the LayoutLines using the ranges inside visual lines
        let number_of_visual_lines = vl_range_of_spans.len();
        for (index, visual_line) in vl_range_of_spans.iter().enumerate() {
            let new_order = self.reorder(&visual_line.ranges);
            let mut glyphs = Vec::with_capacity(1);
            x = start_x;
            y = 0.;
            let alignment_correction = match (align, self.rtl) {
                (Align::Left, true) => line_width as f32 - visual_line.w,
                (Align::Left, false) => 0.,
                (Align::Right, true) => 0.,
                (Align::Right, false) => line_width as f32 - visual_line.w,
                (Align::Center, _) => (line_width as f32 - visual_line.w) / 2.0,
                (Align::Justified, _) => {
                    // Don't justify the last line in a paragraph.
                    if visual_line.spaces > 0 && index != number_of_visual_lines - 1 {
                        (line_width as f32 - visual_line.w) / visual_line.spaces as f32
                    } else {
                        0.
                    }
                }
            };
            if self.rtl {
                if align != Align::Justified {
                    x -= alignment_correction;
                }
                for range in new_order.iter().rev() {
                    for (
                        span_index,
                        (starting_word, starting_glyph),
                        (ending_word, ending_glyph),
                    ) in visual_line.ranges[range.clone()].iter()
                    {
                        let span = &self.spans[*span_index];
                        if starting_word == ending_word {
                            let word_blank = span.words[*starting_word].blank;
                            for glyph in span.words[*starting_word].glyphs
                                [*starting_glyph..*ending_glyph]
                                .iter()
                            {
                                let x_advance = font_size as f32 * glyph.x_advance;
                                let y_advance = font_size as f32 * glyph.y_advance;
                                x -= x_advance;
                                if word_blank && align == Align::Justified {
                                    x -= alignment_correction;
                                }
                                glyphs.push(glyph.layout(font_size, x, y, span.level));
                                y += y_advance;
                            }
                        } else {
                            for i in *starting_word..*ending_word + 1 {
                                if let Some(word) = span.words.get(i) {
                                    let (g1, g2) = if i == *starting_word {
                                        (*starting_glyph, word.glyphs.len())
                                    } else if i == *ending_word {
                                        (0, *ending_glyph)
                                    } else {
                                        (0, word.glyphs.len())
                                    };

                                    let word_blank = word.blank;
                                    for glyph in &word.glyphs[g1..g2] {
                                        let x_advance = font_size as f32 * glyph.x_advance;
                                        let y_advance = font_size as f32 * glyph.y_advance;
                                        if word_blank && align == Align::Justified {
                                            x -= alignment_correction;
                                        }
                                        x -= x_advance;
                                        glyphs.push(glyph.layout(font_size, x, y, span.level));
                                        y += y_advance;
                                    }
                                }
                            }
                        }
                    }
                }
            } else
            /* LTR */
            {
                if align != Align::Justified {
                    x += alignment_correction;
                }
                for range in new_order {
                    for (
                        span_index,
                        (starting_word, starting_glyph),
                        (ending_word, ending_glyph),
                    ) in visual_line.ranges[range.clone()].iter()
                    {
                        let span = &self.spans[*span_index];
                        if starting_word == ending_word {
                            let word_blank = span.words[*starting_word].blank;
                            for glyph in span.words[*starting_word].glyphs
                                [*starting_glyph..*ending_glyph]
                                .iter()
                            {
                                let x_advance = font_size as f32 * glyph.x_advance;
                                let y_advance = font_size as f32 * glyph.y_advance;
                                glyphs.push(glyph.layout(font_size, x, y, span.level));
                                x += x_advance;
                                if word_blank && align == Align::Justified {
                                    x += alignment_correction;
                                }
                                y += y_advance;
                            }
                        } else {
                            for i in *starting_word..*ending_word + 1 {
                                if let Some(word) = span.words.get(i) {
                                    let (g1, g2) = if i == *starting_word {
                                        (*starting_glyph, word.glyphs.len())
                                    } else if i == *ending_word {
                                        (0, *ending_glyph)
                                    } else {
                                        (0, word.glyphs.len())
                                    };

                                    let word_blank = word.blank;
                                    for glyph in &word.glyphs[g1..g2] {
                                        let x_advance = font_size as f32 * glyph.x_advance;
                                        let y_advance = font_size as f32 * glyph.y_advance;
                                        glyphs.push(glyph.layout(font_size, x, y, span.level));
                                        if word_blank && align == Align::Justified {
                                            x += alignment_correction;
                                        }
                                        x += x_advance;
                                        y += y_advance;
                                    }
                                }
                            }
                        }
                    }
                }
            }
            let mut glyphs_swap = Vec::new();
            mem::swap(&mut glyphs, &mut glyphs_swap);
            layout_lines.push(LayoutLine {
                w: if self.rtl { start_x - x } else { x },
                glyphs: glyphs_swap,
            });
            push_line = false;
        }

        if push_line {
            layout_lines.push(LayoutLine {
                w: 0.0,
                glyphs: Default::default(),
            });
        }

        layout_lines
    }
}
