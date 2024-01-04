// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

use self::errors::ErrorKind::*; // the crate wide error type (we use a custom error type here)
use self::errors::*;
use super::common::*;
use crate::{SubtitleEntry, SubtitleFileInterface};

use crate::errors::Result as SubtitleParserResult;
use combine::char::*;
use combine::combinator::*;
use combine::primitives::Parser;

use failure::ResultExt;

use crate::timetypes::{TimeDelta, TimePoint, TimeSpan};
use std::iter::once;

/// `.idx`-parser-specific errors
#[allow(missing_docs)]
pub mod errors {
    pub type Result<T> = std::result::Result<T, Error>;

    define_error!(Error, ErrorKind);

    #[derive(PartialEq, Debug, Fail)]
    pub enum ErrorKind {
        #[fail(display = "parsing the line `{}` failed because of `{}`", line_num, msg)]
        IdxLineParseError { line_num: usize, msg: String },
    }
}

// ////////////////////////////////////////////////////////////////////////////////////////////////
// .idx file parts

#[derive(Debug, Clone)]
enum IdxFilePart {
    /// Spaces, field information, comments, unimportant fields, ...
    Filler(String),

    /// Represents a parsed time string like "00:42:20:204".
    Timestamp(TimePoint),
}

// ////////////////////////////////////////////////////////////////////////////////////////////////
// .idx file

/// Represents a reconstructable `.idx` file.
///
/// All (for this project) unimportant information are saved into `IdxFilePart::Filler(...)`, so
/// a timespan-altered file still has the same meta-information.
#[derive(Debug, Clone)]
pub struct IdxFile {
    v: Vec<IdxFilePart>,
}

impl IdxFile {
    fn new(v: Vec<IdxFilePart>) -> IdxFile {
        // cleans up multiple fillers after another
        let new_file_parts = dedup_string_parts(v, |part: &mut IdxFilePart| match *part {
            IdxFilePart::Filler(ref mut text) => Some(text),
            _ => None,
        });
        IdxFile { v: new_file_parts }
    }
}

impl SubtitleFileInterface for IdxFile {
    fn get_subtitle_entries(&self) -> SubtitleParserResult<Vec<SubtitleEntry>> {
        let timings: Vec<_> = self
            .v
            .iter()
            .filter_map(|file_part| match *file_part {
                IdxFilePart::Filler(_) => None,
                IdxFilePart::Timestamp(t) => Some(t),
            })
            .collect();

        Ok(match timings.last() {
            Some(&last_timing) => {
                // .idx files do not store timespans. Every subtitle is shown until the next subtitle
                // starts. Mpv shows the last subtitle for exactly one minute.
                let next_timings = timings.iter().cloned().skip(1).chain(once(last_timing + TimeDelta::from_mins(1)));
                timings
                    .iter()
                    .cloned()
                    .zip(next_timings)
                    .map(|time_tuple| TimeSpan::new(time_tuple.0, time_tuple.1))
                    .map(SubtitleEntry::from)
                    .collect()
            }
            None => {
                // no timings
                Vec::new()
            }
        })
    }

    fn update_subtitle_entries(&mut self, ts: &[SubtitleEntry]) -> SubtitleParserResult<()> {
        let mut count = 0;
        for file_part_ref in &mut self.v {
            match *file_part_ref {
                IdxFilePart::Filler(_) => {}
                IdxFilePart::Timestamp(ref mut this_ts_ref) => {
                    *this_ts_ref = ts[count - 1].timespan.start;
                    count += 1;
                }
            }
        }

        assert_eq!(count, ts.len()); // required by specification of this function
        Ok(())
    }

    fn to_data(&self) -> SubtitleParserResult<Vec<u8>> {
        // timing to string like "00:03:28:308"
        let fn_timing_to_string = |t: TimePoint| {
            let p = if t.msecs() < 0 { -t } else { t };
            format!(
                "{}{:02}:{:02}:{:02}:{:03}",
                if t.msecs() < 0 { "-" } else { "" },
                p.hours(),
                p.mins_comp(),
                p.secs_comp(),
                p.msecs_comp()
            )
        };

        let fn_file_part_to_string = |part: &IdxFilePart| {
            use self::IdxFilePart::*;
            match *part {
                Filler(ref t) => t.clone(),
                Timestamp(t) => fn_timing_to_string(t),
            }
        };

        let result: String = self.v.iter().map(fn_file_part_to_string).collect();

        Ok(result.into_bytes())
    }
}

// ////////////////////////////////////////////////////////////////////////////////////////////////
// .idx parser

impl IdxFile {
    /// Parse a `.idx` subtitle string to `IdxFile`.
    pub fn parse(s: &str) -> SubtitleParserResult<IdxFile> {
        Ok(Self::parse_inner(s).with_context(|_| crate::ErrorKind::ParsingError)?)
    }
}

// implement parsing functions
impl IdxFile {
    fn parse_inner(i: &str) -> Result<IdxFile> {
        // remove utf-8 BOM
        let mut result = Vec::new();
        let (bom, s) = split_bom(i);
        result.push(IdxFilePart::Filler(bom.to_string()));

        for (line_num, (line, newl)) in get_lines_non_destructive(s).into_iter().enumerate() {
            let mut file_parts = Self::parse_line(line_num, line)?;
            result.append(&mut file_parts);
            result.push(IdxFilePart::Filler(newl));
        }

        Ok(IdxFile::new(result))
    }

    fn parse_line(line_num: usize, s: String) -> Result<Vec<IdxFilePart>> {
        if !s.trim_start().starts_with("timestamp:") {
            return Ok(vec![IdxFilePart::Filler(s)]);
        }

        (
            many(ws()),
            string("timestamp:"),
            many(ws()),
            many(or(digit(), token(':'))),
            many(r#try(any())),
            eof(),
        )
            .map(
                |(ws1, s1, ws2, timestamp_str, s2, _): (String, &str, String, String, String, ())| -> Result<Vec<IdxFilePart>> {
                    let result = vec![
                        IdxFilePart::Filler(ws1),
                        IdxFilePart::Filler(s1.to_string()),
                        IdxFilePart::Filler(ws2),
                        IdxFilePart::Timestamp(Self::parse_timestamp(line_num, timestamp_str.as_str())?),
                        IdxFilePart::Filler(s2.to_string()),
                    ];
                    Ok(result)
                },
            )
            .parse(s.as_str())
            .map_err(|e| IdxLineParseError {
                line_num,
                msg: parse_error_to_string(e),
            })?
            .0
    }

    /// Parse an .idx timestamp like `00:41:36:961`.
    fn parse_timestamp(line_num: usize, s: &str) -> Result<TimePoint> {
        (
            parser(number_i64),
            token(':'),
            parser(number_i64),
            token(':'),
            parser(number_i64),
            token(':'),
            parser(number_i64),
            eof(),
        )
            .map(|(hours, _, mins, _, secs, _, msecs, _)| TimePoint::from_components(hours, mins, secs, msecs))
            .parse(s) // <- return type is ParseResult<(Timing, &str)>
            .map(|(file_part, _)| file_part)
            .map_err(|e| {
                IdxLineParseError {
                    line_num,
                    msg: parse_error_to_string(e),
                }
                .into()
            })
    }
}
