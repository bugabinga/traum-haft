use spacetimedb::{Identity, ReducerContext, Table};

/// What SpacetimeDB tells the module about each caller.
#[spacetimedb::table(accessor = seen, public)]
pub struct Seen {
    #[primary_key]
    #[auto_inc]
    pub id: u64,
    pub identity: Identity,
    pub iss: String,
    pub sub: String,
    pub aud: String,
}

#[spacetimedb::reducer]
pub fn whoami(ctx: &ReducerContext) -> Result<(), String> {
    let auth = ctx.sender_auth();
    let jwt = auth.jwt().ok_or("no jwt")?;
    ctx.db.seen().insert(Seen {
        id: 0,
        identity: ctx.sender(),
        iss: jwt.issuer().to_string(),
        sub: jwt.subject().to_string(),
        aud: jwt.audience().join(","),
    });
    Ok(())
}
