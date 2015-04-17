//! Data model and PostgreSQL storage abstraction.

use std::fmt::{Display, Formatter};

use postgres::Connection;

use error::FictError;

/// Participant in the collaborative storytelling process. Automatically created on first oauth
/// login.
pub struct User {
    id: Option<i64>,
    name: String,
    email: String,
}

impl User {
    /// Create the database table used to store `User` instances. Do nothing if it already
    /// exists.
    fn initialize(conn: &Connection) -> Result<(), FictError> {
        try!(conn.execute("CREATE TABLE users IF NOT EXISTS (
            id SERIAL PRIMARY KEY,
            name VARCHAR NOT NULL,
            email VARCHAR NOT NULL
        )", &[]));

        try!(conn.execute("CREATE UNIQUE INDEX email_index ON users (email)", &[]));

        Ok(())
    }

    /// Persist any local modifications to this `User` to the database.
    fn save(&mut self, conn: &Connection) -> Result<(), FictError> {
        match self.id {
            Some(existing_id) => {
                try!(conn.execute("
                    UPDATE users
                    SET name = $1, email = $2
                    WHERE id = $3
                ", &[&self.name, &self.email, existing_id]));
                Ok(())
            },
            None => {
                let row = try!(conn.prepare("
                    INSERT INTO users (name, email)
                    VALUES ($1, $2)
                    RETURNING id
                ").map(|insertion| insertion.query(&[&self.name, &self.email]))).next();
                self.id = Some(row.get(0));
                Ok(())
            },
        }
    }

    /// Discover an existing `User` by email address. If none exists, create, persist, and return a
    /// new one with the provided `name`.
    fn find_or_create(conn: &Connection, email: String, name: String) -> Result<User, FictError> {
        let results = try!(conn.prepare("
            SELECT id, name, email FROM users
            WHERE email = $1
        ").map(|statement| statement.query(&[&email])));

        let user = match results.next() {
            Some(row) => {
                User{
                    id: Some(row.get(0)),
                    name: row.get(1),
                    email: row.get(2),
                };
            },
            None => {
                let mut u = User{id: None, name: name, email: email};
                try!(u.save(conn));
                u
            },
        };
        Ok(user)
    }
}
