use rusqlite::{self, Connection, Result, Transaction};
use std::ffi::OsString;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use tree_sitter::Point;

pub struct Store {
    db: Connection,
    path: PathBuf,
}

pub struct StoreFile<'a> {
    file_id: i64,
    db: Transaction<'a>,
}

impl Store {
    pub fn new(db_path: PathBuf) -> rusqlite::Result<Self> {
        let db = Connection::open(&db_path)?;
        db.set_prepared_statement_cache_capacity(20);
        Ok(Self { db, path: db_path })
    }

    pub fn clone(&self) -> rusqlite::Result<Self> {
        Self::new(self.path.clone())
    }

    pub fn initialize(&mut self) -> rusqlite::Result<()> {
        self.db.execute_batch(include_str!("./schema.sql"))
    }

    pub fn delete_files(&mut self, path: &Path) -> rusqlite::Result<()> {
        self.db.execute(
            "DELETE FROM files WHERE instr(path, ?1) = 1",
            &[&path.as_os_str().as_bytes()]
        )?;
        Ok(())
    }

    pub fn file(&mut self, path: &Path) -> rusqlite::Result<StoreFile> {
        let tx = self.db.transaction()?;
        {
            let mut stmt = tx.prepare_cached("DELETE FROM files WHERE path = ?1")?;
            stmt.execute(&[&path.as_os_str().as_bytes()])?;
            let mut stmt = tx.prepare_cached("INSERT INTO files (path) VALUES (?1)")?;
            stmt.execute(&[&path.as_os_str().as_bytes()])?;
        }
        let file_id = tx.last_insert_rowid();
        Ok(StoreFile { file_id, db: tx })
    }

    pub fn find_definition(
        &mut self,
        path: &Path,
        position: Point,
    ) -> Result<Vec<(PathBuf, Point, usize)>> {
        let file_id: i64 = self.db.query_row(
            "SELECT id FROM files WHERE path = ?1",
            &[&path.as_os_str().as_bytes()],
            |row| row.get(0),
        )?;

        let local_result = self.db.query_row(
            "
                SELECT
                    local_defs.row,
                    local_defs.column,
                    local_defs.length
                FROM
                    local_refs,
                    local_defs
                WHERE
                    local_refs.definition_id = local_defs.id AND
                    local_refs.file_id = ?1 AND
                    local_refs.row = ?2 AND
                    local_refs.column <= ?3 AND
                    local_refs.column + local_refs.length > ?3
            ",
            &[&file_id, &(position.row as i64), &(position.column as i64)],
            |row| {
                (
                    Point {
                        row: row.get(0),
                        column: row.get(1),
                    },
                    row.get::<usize, i64>(2),
                )
            },
        );

        match local_result {
            Err(rusqlite::Error::QueryReturnedNoRows) => {}
            Ok((position, length)) => return Ok(vec![(path.to_owned(), position, length as usize)]),
            Err(e) => return Err(e.into()),
        }

        let mut statement = self.db.prepare_cached(
            "
                SELECT
                    files.path,
                    defs.name_start_row,
                    defs.name_start_column,
                    length(defs.name)
                FROM
                    files,
                    defs,
                    refs
                WHERE
                    files.id == defs.file_id AND
                    defs.name = refs.name AND
                    refs.file_id = ?1 AND
                    refs.row = ?2 AND
                    refs.column <= ?3 AND
                    refs.column + length(refs.name) > ?3
                LIMIT
                    50
            ",
        )?;

        let rows = statement.query_map(
            &[&file_id, &(position.row as i64), &(position.column as i64)],
            |row| {
                (
                    OsString::from_vec(row.get::<usize, Vec<u8>>(0)).into(),
                    Point::new(row.get(1), row.get(2)),
                    row.get::<usize, i64>(3) as usize,
                )
            },
        )?;

        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }

        Ok(result)
    }
}

impl<'a> StoreFile<'a> {
    pub fn insert_local_ref(
        &mut self,
        local_def_id: i64,
        name: &'a str,
        position: Point,
    ) -> Result<()> {
        let mut stmt = self.db.prepare_cached(
            "
                INSERT INTO local_refs
                (file_id, definition_id, row, column, length)
                VALUES
                (?1, ?2, ?3, ?4, ?5)
            ",
        )?;
        stmt.execute(&[
            &self.file_id,
            &local_def_id,
            &position.row,
            &position.column,
            &(name.as_bytes().len() as i64),
        ])?;
        Ok(())
    }

    pub fn insert_local_def(&mut self, name: &'a str, position: Point) -> Result<i64> {
        let mut stmt = self.db.prepare_cached(
            "
                INSERT INTO local_defs
                (file_id, row, column, length)
                VALUES
                (?1, ?2, ?3, ?4)
            ",
        )?;
        stmt.execute(&[
            &self.file_id,
            &position.row,
            &position.column,
            &(name.as_bytes().len() as i64),
        ])?;
        Ok(self.db.last_insert_rowid())
    }

    pub fn insert_ref(
        &mut self,
        name: &'a str,
        position: Point,
        kind: Option<&'a str>,
    ) -> Result<()> {
        let mut stmt = self.db.prepare_cached(
            "
                INSERT INTO refs
                (file_id, name, row, column, kind)
                VALUES
                (?1, ?2, ?3, ?4, ?5)
            ",
        )?;
        stmt.execute(&[&self.file_id, &name, &position.row, &position.column, &kind])?;
        Ok(())
    }

    pub fn insert_def(
        &mut self,
        name: &'a str,
        name_position: Point,
        start_position: Point,
        end_position: Point,
        kind: Option<&'a str>,
        module_path: &Vec<&'a str>,
    ) -> Result<()> {
        let mut module_path_string = String::with_capacity(
            module_path
                .iter()
                .map(|entry| entry.as_bytes().len() + 1)
                .sum(),
        );
        for entry in module_path {
            module_path_string += entry;
            module_path_string += "\t";
        }
        let mut stmt = self.db.prepare_cached(
            "
                INSERT INTO defs
                (
                    file_id,
                    start_row, start_column,
                    end_row, end_column,
                    name, name_start_row, name_start_column,
                    kind,
                    module_path
                )
                VALUES
                (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
            ",
        )?;
        stmt.execute(&[
            &self.file_id,
            &start_position.row,
            &start_position.column,
            &end_position.row,
            &end_position.column,
            &name,
            &name_position.row,
            &name_position.column,
            &kind,
            &module_path_string,
        ])?;
        Ok(())
    }

    pub fn commit(self) -> rusqlite::Result<()> {
        self.db.commit()
    }
}
