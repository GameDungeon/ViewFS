use nom::bytes::complete::is_not;
use nom::character::complete::{char, line_ending, not_line_ending, space0};
use nom::combinator::{opt, value};
use nom::multi::separated_list0;
use nom::sequence::preceded;
use nom::IResult;
use nom::Parser;

use anyhow::Result;

use std::fs;
use std::path::PathBuf;

// Matches a comment
fn comment(s: &str) -> IResult<&str, ()> {
    value((), preceded(char('#'), not_line_ending)).parse(s)
}

// Matches until a line ending or comment
fn path(input: &str) -> IResult<&str, &str> {
    let (input, s) = is_not("\r\n#")(input)?;
    Ok((input, s.trim_end()))
}

// Matches a full line
fn line(input: &str) -> IResult<&str, Option<String>> {
    let (input, _) = space0(input)?;
    let (input, p) = opt(path).parse(input)?;
    let (input, _) = space0(input)?;
    let (input, _) = opt(comment).parse(input)?;

    Ok((input, p.map(|s| s.to_string())))
}

fn parser(s: &str) -> IResult<&str, Vec<String>> {
    let (input, lines) = separated_list0(line_ending, line).parse(s)?;

    let (input, _) = opt(line_ending).parse(input)?;
    let paths = lines.into_iter().flatten().collect();
    Ok((input, paths))
}

pub fn parse_filter(file_path: PathBuf) -> Result<Vec<PathBuf>> {
    let filters_raw = fs::read_to_string(file_path)?;
    let (_, paths) = parser(&filters_raw)
        .map_err(|e| anyhow::anyhow!("Parse error: {:?}", e))?;

    let mut result = Vec::new();
    for path_str in paths {
        let p = PathBuf::from(&path_str);
        if !p.is_absolute() {
            anyhow::bail!("Path is not absolute: {}", path_str);
        }
        let canonical = p
            .canonicalize()
            .map_err(|e| anyhow::anyhow!("Failed to canonicalize '{}': {}", path_str, e))?;
        result.push(canonical);
    }
    Ok(result)
}
