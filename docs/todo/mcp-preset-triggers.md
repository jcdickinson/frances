# Automatic MCP preset activation

Preset composition is part of the initial MCP implementation: users can select
multiple presets together, such as `frances + rust`.

Later, consider workspace triggers for presets. For example, detecting a
`Cargo.toml` could activate the `rust` preset alongside the user's selections.

Open questions:

- Should a trigger activate a preset automatically or suggest it to the user?
- When and where should detection run, including multi-root workspaces?
- How can users override or disable triggers?

The Frances workflow must remain an explicit user choice.
