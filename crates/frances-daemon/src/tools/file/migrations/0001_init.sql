CREATE TABLE IF NOT EXISTS file_meta (
    path           TEXT PRIMARY KEY,
    mtime_ns       INTEGER NOT NULL,
    size           INTEGER NOT NULL,
    content_digest INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS file_lines (
    path    TEXT    NOT NULL,
    line_no INTEGER NOT NULL,
    hash    INTEGER NOT NULL,
    anchor  BLOB    NOT NULL,
    PRIMARY KEY(path, line_no)
);

CREATE TABLE IF NOT EXISTS file_tombstones (
    path   TEXT NOT NULL,
    anchor BLOB NOT NULL,
    PRIMARY KEY(path, anchor)
);
