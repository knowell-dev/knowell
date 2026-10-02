//! Prints the size of the tool listing that MCP clients place in the model's context.
//!
//! `input+desc` (name, description and input schema) is what the model pays
//! for on every turn; `knowell_mcp::tool_definitions` is held to a budget by
//! the `model_facing_size_stays_within_budget` test.

#![allow(clippy::print_stdout, clippy::print_stderr)]

fn main() {
    let tools = match knowell_mcp::tool_definitions() {
        Ok(tools) => tools,
        Err(err) => {
            eprintln!("{err}");
            std::process::exit(1);
        }
    };
    let mut input = 0usize;
    let mut total = 0usize;
    for tool in &tools {
        let full = serde_json::to_string(tool).unwrap_or_default();
        let schema = serde_json::to_string(&tool.input_schema).unwrap_or_default();
        let desc = tool.description.as_deref().map_or(0, str::len);
        let model_facing = schema.len() + desc + tool.name.len();
        input += model_facing;
        total += full.len();
        println!(
            "{:<16} input+desc {:>6} B   (schema {:>5}, description {:>4})   full {:>6} B",
            tool.name,
            model_facing,
            schema.len(),
            desc,
            full.len()
        );
    }
    println!(
        "tools: {}  input+desc: {} B (~{} tokens)  full listing: {} B",
        tools.len(),
        input,
        input / 4,
        total
    );
}
