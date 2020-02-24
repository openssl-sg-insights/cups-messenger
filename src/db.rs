use ed25519_dalek::PublicKey;
use failure::Error;
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::params;
use rusqlite::OptionalExtension;
use uuid::Uuid;

use crate::message::{NewInboundMessage, NewOutboundMessage};
use crate::query::BeforeAfter;
use crate::query::Limits;

pub type DbPool = Pool<SqliteConnectionManager>;
lazy_static::lazy_static! {
    pub static ref POOL: DbPool = Pool::new(SqliteConnectionManager::file("messages.db")).expect("POOL");
}

pub async fn get_message_count_by_user(pubkey: PublicKey) -> Result<i64, Error> {
    let pool = POOL.clone();
    let res = tokio::task::spawn_blocking(move || {
        let conn = pool.get()?;
        let res = conn.query_row(
            "SELECT count(id) FROM messages WHERE user_id = ?1",
            params![&pubkey.as_bytes()[..]],
            |row| row.get(0),
        )?;
        Ok::<_, Error>(res)
    })
    .await??;
    Ok(res)
}

pub async fn save_in_message(message: NewInboundMessage) -> Result<(), Error> {
    let pool = POOL.clone();
    tokio::task::spawn_blocking(move || {
        let conn = pool.get()?;
        conn.execute(
            "INSERT INTO messages (user_id, inbound, time, content) VALUES (?1, true, ?2, ?3)",
            params![&message.from.as_bytes()[..], message.time, message.content],
        )?;
        Ok::<_, Error>(())
    })
    .await??;
    Ok(())
}

pub async fn save_out_message(message: NewOutboundMessage) -> Result<(), Error> {
    let pool = POOL.clone();
    tokio::task::spawn_blocking(move || {
        let conn = pool.get()?;
        conn.execute(
            "INSERT INTO messages (tracking_id, user_id, inbound, time, content, read) VALUES (?1, ?2, false, ?3, ?4, true)",
            params![message.tracking_id, &message.to.as_bytes()[..], message.time, message.content],
        )?;
        Ok::<_, Error>(())
    })
    .await??;
    Ok(())
}

pub async fn save_user(pubkey: PublicKey, name: String) -> Result<(), Error> {
    let pool = POOL.clone();
    tokio::task::spawn_blocking(move || {
        let conn = pool.get()?;
        conn.execute(
            "INSERT INTO users (id, name) VALUES (?1, ?2) ON CONFLICT(id) DO UPDATE SET name = excluded.name",
            params![&pubkey.as_bytes()[..], name],
        )?;
        Ok::<_, Error>(())
    })
    .await??;
    Ok(())
}

pub async fn get_user(pubkey: PublicKey) -> Result<Option<String>, Error> {
    let pool = POOL.clone();
    let res = tokio::task::spawn_blocking(move || {
        let conn = pool.get()?;
        let res = conn
            .query_row(
                "SELECT name FROM users WHERE id = ?1",
                params![&pubkey.as_bytes()[..]],
                |row| row.get(0),
            )
            .optional()?;
        Ok::<_, Error>(res)
    })
    .await??;
    Ok(res)
}

pub async fn del_user(pubkey: PublicKey) -> Result<(), Error> {
    let pool = POOL.clone();
    let res = tokio::task::spawn_blocking(move || {
        let conn = pool.get()?;
        conn.execute(
            "DELETE FROM users WHERE id = ?1",
            params![&pubkey.as_bytes()[..]],
        )?;
        Ok::<_, Error>(())
    })
    .await??;
    Ok(res)
}

#[derive(Clone, Debug)]
pub struct UserInfo {
    pub pubkey: PublicKey,
    pub name: Option<String>,
    pub unreads: i64,
}

pub async fn get_user_info() -> Result<Vec<UserInfo>, Error> {
    let pool = POOL.clone();
    let res = tokio::task::spawn_blocking(move || {
        let conn = pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT
                messages.user_id,
                users.name,
                SUM(CASE WHEN messages.read THEN 0 ELSE 1 END)
            FROM messages
            LEFT JOIN users
            ON messages.user_id = users.id
            GROUP BY users.id, users.name
            UNION ALL
            SELECT
                users.id,
                users.name,
                count(messages.id)
            FROM users
            LEFT JOIN messages
            ON messages.user_id = users.id
            WHERE messages.user_id IS NULL
            GROUP BY users.id, users.name",
        )?;
        let res = stmt
            .query_map(params![], |row| {
                let uid: Vec<u8> = row.get(0)?;
                Ok(UserInfo {
                    pubkey: PublicKey::from_bytes(&uid).map_err(|e| {
                        rusqlite::Error::FromSqlConversionFailure(
                            0,
                            rusqlite::types::Type::Blob,
                            Box::new(e),
                        )
                    })?,
                    name: row.get(1)?,
                    unreads: row.get(2)?,
                })
            })?
            .collect::<Result<_, _>>()?;
        Ok::<_, Error>(res)
    })
    .await??;
    Ok(res)
}

#[derive(Clone, Debug)]
pub struct Message {
    pub id: i64,
    pub tracking_id: Option<Uuid>,
    pub time: i64,
    pub inbound: bool,
    pub content: String,
}
pub async fn get_messages(
    pubkey: PublicKey,
    limits: Limits,
    mark_as_read: bool,
) -> Result<Vec<Message>, Error> {
    let pool = POOL.clone();
    let res = tokio::task::spawn_blocking(move || {
        let mut gconn = pool.get()?;
        let conn = gconn.transaction()?;
        if mark_as_read {
            conn.execute(
                &format!("UPDATE messages SET read = true WHERE user_id = ?1 AND id IN (SELECT id FROM messages WHERE user_id = ?1{}{} ORDER BY created_at DESC LIMIT {})",
                if let Some(BeforeAfter::Before(before)) = &limits.before_after { format!(" AND id < {}", before)} else { "".to_owned() },
                if let Some(BeforeAfter::After(after)) = &limits.before_after { format!(" AND id > {}", after) } else { "".to_owned() },
                limits.limit.unwrap_or(1024)),
                params![&pubkey.as_bytes()[..]]
            )?;
        }
        let mut stmt = conn.prepare(
            &format!("SELECT id, tracking_id, time, inbound, content FROM messages WHERE user_id = ?1 {} LIMIT {}",
            match &limits.before_after {
                Some(BeforeAfter::Before(before)) => format!("AND id < {} ORDER BY created_at DESC", before),
                Some(BeforeAfter::After(after)) => format!("AND id > {} ORDER BY created_at ASC", after),
                None => format!("ORDER BY created_at DESC"),
            },
            limits.limit.unwrap_or(1024)),
        )?;
        let res = stmt
            .query_map(params![&pubkey.as_bytes()[..]], |row| {
                Ok(Message {
                    id: row.get(0)?,
                    tracking_id: row.get(1)?,
                    time: row.get(2)?,
                    inbound: row.get(3)?,
                    content: row.get(4)?,
                })
            })?
            .collect::<Result<_, _>>()?;
        drop(stmt);
        conn.commit()?;
        Ok::<_, Error>(res)
    })
    .await??;
    Ok(res)
}
