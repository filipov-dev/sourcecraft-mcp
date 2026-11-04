//! `sourcecraft-mcp toolsets` — список наборов инструментов для `--toolsets`.

use sourcecraft_mcp_core::{Toolset, available_toolsets};

pub fn run() -> anyhow::Result<()> {
    print!("{}", render(&available_toolsets()));
    Ok(())
}

/// Таблица: slug, число инструментов, исходный тег спецификации.
fn render(toolsets: &[Toolset]) -> String {
    let width = toolsets
        .iter()
        .map(|t| t.slug.len())
        .max()
        .unwrap_or(0)
        .max("TOOLSET".len());
    let mut out = format!("{:<width$}  TOOLS  API TAG\n", "TOOLSET");
    for t in toolsets {
        out.push_str(&format!(
            "{:<width$}  {:>5}  {}\n",
            t.slug, t.operations, t.tag
        ));
    }
    out.push_str(
        "\nPass a comma-separated list to `serve --toolsets` or `connect --toolsets`. \
         Tools above the allowed access level stay hidden regardless of toolsets.\n",
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_all_toolsets() {
        let text = render(&available_toolsets());
        assert!(text.starts_with("TOOLSET"));
        assert!(text.contains("repository-pullrequest"));
        assert!(text.contains("Repository | PullRequest"));
    }
}
