//! Create and manage active user sessions.

use std::fmt::{self, Display, Formatter};

use postgres::Connection;
use rand::Rng;

use model::{User, first, first_opt};
use error::FictResult;

/// An active user of the site.
pub struct Session {
    pub id: i64,
    pub token: i64,
    user_id: i64,
}

impl Session {
    /// Create the database table used to store `Sessions`, if necessary.
    pub fn initialize(conn: &Connection) -> FictResult<()> {
        try!(conn.execute("
            CREATE TABLE IF NOT EXISTS sessions (
                id BIGSERIAL PRIMARY KEY,
                token BIGINT NOT NULL,
                user_id BIGINT NOT NULL REFERENCES users (id)
                    ON DELETE CASCADE
                    ON UPDATE CASCADE
            )
        ", &[]));

        try!(conn.execute("
            CREATE UNIQUE INDEX IF NOT EXISTS token_index ON sessions (token)
        ", &[]));

        Ok(())
    }

    /// Assign a new Session to a newly logged-in User.
    ///
    /// Panics if the User has not been persisted.
    pub fn assign<R: Rng>(conn: &Connection, u: User, rng: &mut R) -> FictResult<Session> {
        let token = rng.gen::<i64>();
        let user_id = u.id.unwrap();

        let insertion = try!(conn.prepare("
            INSERT INTO sessions (token, user_id)
            VALUES ($1, $2)
            RETURNING id
        "));
        let rows = try!(insertion.query(&[&token, &user_id]));
        let row = try!(first(&rows));

        Ok(Session{
            id: row.get(0),
            token: token,
            user_id: user_id,
        })
    }

    /// Given an API token from a request, attempt to locate the created Session. Returns
    /// `Some(Session)` if a Session is found, `Ok(None)` if no Session with a matching token
    /// exists, or an `Err` if there's some problem checking the database.
    pub fn validate(conn: &Connection, token: i64) -> FictResult<Option<Session>> {
        let selection = try!(conn.prepare("
            SELECT id, token, user_id FROM sessions
            WHERE token = $1
        "));

        let rows = try!(selection.query(&[&token]));
        let row_opt = try!(first_opt(&rows));

        Ok(row_opt.map(|row| Session{
            id: row.get(0),
            token: row.get(1),
            user_id: row.get(2),
        }))
    }

    /// Access the User corresponding to this Session.
    pub fn user(&self, conn: &Connection) -> FictResult<User> {
        User::with_id(conn, self.user_id)
    }
}

impl Display for Session {
    fn fmt(&self, f: &mut Formatter) -> Result<(), fmt::Error> {
        write!(f, "Session(id=[{}] user_id=[{}] token=[..])",
            self.id, self.user_id)
    }
}
