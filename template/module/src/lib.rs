//! Example app: a shared notice board. Replace freely, but keep the
//! `client_connected` check and call `visitor(ctx)` in reducers that need a
//! signed-in coworker.

use spacetimedb::{Identity, ReducerContext, Table, Timestamp};
use traum_haft_module::visitor;

#[spacetimedb::table(accessor = note, public)]
pub struct Note {
    #[primary_key]
    #[auto_inc]
    pub id: u64,
    pub author: Identity,
    pub author_email: String,
    pub text: String,
    pub created_at: Timestamp,
}

/// Refuses connections that the platform did not sign in for this app.
#[spacetimedb::reducer(client_connected)]
pub fn on_connect(ctx: &ReducerContext) -> Result<(), String> {
    visitor(ctx).map(|_| ())
}

#[spacetimedb::reducer]
pub fn add_note(ctx: &ReducerContext, text: String) -> Result<(), String> {
    let me = visitor(ctx)?;
    let text = text.trim().to_string();
    if text.is_empty() || text.chars().count() > 500 {
        return Err("Notes need 1 to 500 characters".into());
    }
    ctx.db.note().insert(Note {
        id: 0,
        author: me.identity,
        author_email: me.email,
        text,
        created_at: ctx.timestamp,
    });
    Ok(())
}

#[spacetimedb::reducer]
pub fn delete_note(ctx: &ReducerContext, id: u64) -> Result<(), String> {
    let me = visitor(ctx)?;
    let note = ctx.db.note().id().find(id).ok_or("No such note")?;
    if note.author != me.identity {
        return Err("Only the author can delete a note".into());
    }
    ctx.db.note().id().delete(id);
    Ok(())
}
