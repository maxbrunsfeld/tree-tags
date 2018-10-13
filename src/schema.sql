CREATE TABLE IF NOT EXISTS files (
  id INTEGER PRIMARY KEY NOT NULL,
  path TEXT NOT NULL
);

create table if not exists local_definitions (
  id INTEGER NOT NULL PRIMARY KEY,
  file_id INTEGER NOT NULL,
  row UNSIGNED INTEGER NOT NULL,
  column UNSIGNED INTEGER NOT NULL,
  length UNSIGNED INTEGER NOT NULL,
  FOREIGN KEY(file_id) REFERENCES files(id)
);

create table if not exists local_references (
  file_id INTEGER NOT NULL,
  definition_id INTEGER NOT NULL,
  row UNSIGNED INTEGER NOT NULL,
  column UNSIGNED INTEGER NOT NULL,
  length UNSIGNED INTEGER NOT NULL,
  PRIMARY KEY(file_id, row, column),
  FOREIGN KEY(file_id) REFERENCES files(id),
  FOREIGN KEY(definition_id) REFERENCES local_definitions(id)
);
