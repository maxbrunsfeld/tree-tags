PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS files (
  id INTEGER NOT NULL PRIMARY KEY,
  path TEXT NOT NULL UNIQUE
);

create table if not exists local_defs (
  id INTEGER NOT NULL PRIMARY KEY,
  file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
  row UNSIGNED INTEGER NOT NULL,
  column UNSIGNED INTEGER NOT NULL,
  length UNSIGNED INTEGER NOT NULL
);

create table if not exists local_refs (
  file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
  definition_id INTEGER NOT NULL REFERENCES local_defs(id) ON DELETE CASCADE,
  row UNSIGNED INTEGER NOT NULL,
  column UNSIGNED INTEGER NOT NULL,
  length UNSIGNED INTEGER NOT NULL,
  PRIMARY KEY(file_id, row, column)
);

create table if not exists defs (
  file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
  start_row UNSIGNED INTEGER NOT NULL,
  start_column UNSIGNED INTEGER NOT NULL,
  name_start_row UNSIGNED INTEGER NOT NULL,
  name_start_column UNSIGNED INTEGER NOT NULL,
  end_row UNSIGNED INTEGER NOT NULL,
  end_column UNSIGNED INTEGER NOT NULL,
  name TEXT NOT NULL,
  kind TEXT NOT NULL,
  module_path TEXT NOT NULL,
  PRIMARY KEY(file_id, start_row, start_column, end_row, end_column)
);

create table if not exists refs (
  file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
  row UNSIGNED INTEGER NOT NULL,
  column UNSIGNED INTEGER NOT NULL,
  name TEXT NOT NULL,
  kind TEXT NOT NULL,
  PRIMARY KEY(file_id, row, column)
);
