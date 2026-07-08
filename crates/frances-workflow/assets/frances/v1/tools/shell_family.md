## Shell tools — quasi-persistent state

Commands use quasi-persistent bash state. The working directory always
persists after completed shell_run calls. Exported environment variables
persist only when a shell_run call includes their names in `persist`;
that `persist` list applies to that one run and is not a durable watch list.
FRANCES_ROOT is reserved and Frances-managed; persisted environment cannot
override it. Prefer dedicated tools (`file_read`, `file_replace_lines`,
`variable_*`) over shell equivalents (`cat`, `echo`, `jq`) when available.
