use anyhow::{bail, Context};
use serde::Serialize;

pub(super) const MAX_PAGE_BYTES: usize = 8 * 1024 * 1024;

pub(super) fn slice<T: Serialize>(
    items: Vec<T>,
    offset: u32,
    limit: usize,
    mut bytes_left: usize,
    next: &mut Option<u32>,
) -> anyhow::Result<Vec<T>> {
    let mut items = items.into_iter().skip(offset as usize).peekable();
    let mut result = Vec::new();
    while result.len() < limit {
        let Some(item) = items.peek() else {
            break;
        };
        let bytes = serde_json::to_vec(&item)?.len() + 1;
        if bytes > bytes_left {
            if result.is_empty() {
                bail!("a single knowledge card exceeds the 8 MiB page limit");
            }
            break;
        }
        bytes_left -= bytes;
        result.push(items.next().expect("peeked knowledge card"));
    }
    if items.peek().is_some() {
        *next = Some(
            offset
                .checked_add(result.len() as u32)
                .context("knowledge page offset overflow")?,
        );
    }
    Ok(result)
}
