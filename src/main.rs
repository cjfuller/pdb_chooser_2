use anyhow::Result;
use regex::Regex;
use rocket::form::{Form, FromForm};
use rocket::fs::NamedFile;
use rocket::request::FromParam;
use rocket::{get, launch, post, routes};
use rocket::{Responder, State};
use rocket_dyn_templates::Template;
use serde::{Deserialize, Serialize};
use sqlx::sqlite::{SqliteConnectOptions, SqliteSynchronous};
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::str::FromStr;

const DB_FILE: &str = "claims.sqlite";

lazy_static::lazy_static! {
    static ref SECRET_KEY: String = std::fs::read_to_string("./secret_key.txt").unwrap().trim().to_string();
    static ref ADMIN_SECRET_KEY: String = std::fs::read_to_string("./admin_secret_key.txt").unwrap().trim().to_string();
    static ref PDB_RE: Regex = Regex::new(r"^\d[A-Z0-9]{3}$").unwrap();
    static ref MCG_ID_RE: Regex = Regex::new(r"^\d{9}$").unwrap();
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug)]
struct Student<'a> {
    pub name: &'a str,
    pub id: &'a str,
}

async fn make_id_claim(pool: &SqlitePool, pdb: &str, student: Student<'_>) -> bool {
    sqlx::query(
        r#"
        INSERT INTO pdb_claims (pdb, mcgill_id, name)
        VALUES ($1, $2, $3)
    "#,
    )
    .bind(pdb)
    .bind(student.id)
    .bind(student.name)
    .execute(pool)
    .await
    .is_ok()
}

#[derive(Debug, Serialize, FromForm, sqlx::FromRow)]
struct TemplateParams {
    name: String,
    pdb: String,
    mcgill_id: String,
}

impl TemplateParams {
    pub fn normalize(&mut self) {
        self.pdb = self.pdb.to_uppercase();
    }
}

#[derive(Debug, Serialize, FromForm)]
struct CheckParams {
    mcgill_id: String,
}

#[derive(Debug)]
struct InvalidKey;

enum SecretKey {
    User,
    Admin,
}

impl<'a> FromParam<'a> for SecretKey {
    type Error = InvalidKey;

    fn from_param(param: &'a str) -> std::result::Result<Self, Self::Error> {
        if param == *SECRET_KEY {
            Ok(Self::User)
        } else if param == *ADMIN_SECRET_KEY {
            Ok(Self::Admin)
        } else {
            Err(InvalidKey)
        }
    }
}

struct InternalServerError {
    wrapped: anyhow::Error,
}

impl<E> From<E> for InternalServerError
where
    E: Into<anyhow::Error>,
{
    fn from(wrapped: E) -> Self {
        InternalServerError {
            wrapped: wrapped.into(),
        }
    }
}

impl<'a> rocket::response::Responder<'a, 'a> for InternalServerError {
    fn respond_to(self, request: &rocket::request::Request<'_>) -> rocket::response::Result<'a> {
        rocket::response::Debug(self.wrapped).respond_to(request)
    }
}

#[get("/<secret_key>")]
#[allow(unused_variables)]
async fn index(secret_key: SecretKey) -> Template {
    let mut params = HashMap::new();
    params.insert("name", "");
    params.insert("pdb", "");
    params.insert("mcgill_id", "");
    params.insert("secret_key", &SECRET_KEY);
    Template::render("index", params)
}

#[derive(Responder, Debug)]
enum RegisterResponse {
    Success(Template),
    #[response(status = 400)]
    Conflict(Template),
}

async fn already_registered(pool: &SqlitePool, mcgill_id: &str) -> Result<Option<String>> {
    let row: Option<(String,)> = sqlx::query_as("SELECT pdb FROM pdb_claims WHERE mcgill_id = $1;")
        .bind(mcgill_id)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(|it| it.0))
}

#[post("/<secret_key>", data = "<reg>")]
#[allow(unused_variables)]
async fn register(
    secret_key: SecretKey,
    mut reg: Form<TemplateParams>,
    pool: &State<SqlitePool>,
) -> Result<RegisterResponse, InternalServerError> {
    println!("{:?}", reg);
    reg.normalize();
    let mut feedback = HashMap::new();
    if reg.name.is_empty() {
        feedback.insert(
            "name_validation_msg",
            "Please enter a valid name.".to_string(),
        );
    }
    if !PDB_RE.is_match(&reg.pdb) {
        feedback.insert(
            "pdb_validation_msg",
            "Please enter a valid PDB id (e.g. 2ARO).".to_string(),
        );
    }

    if !MCG_ID_RE.is_match(&reg.mcgill_id) {
        feedback.insert(
            "mcgill_id_validation_msg",
            "Please enter a valid 9-digit McGill ID.".to_string(),
        );
    }

    if feedback.is_empty() {
        let claim = make_id_claim(
            pool,
            &reg.pdb,
            Student {
                id: &reg.mcgill_id,
                name: &reg.name,
            },
        )
        .await;
        if !claim {
            let existing = already_registered(pool, &reg.mcgill_id).await?;
            if let Some(pdb_id) = existing {
                feedback.insert(
                    "pdb_validation_msg",
                    format!("You have already registered PDB id {pdb_id}"),
                );
            } else {
                feedback.insert(
                    "pdb_validation_msg",
                    "This PDB id is already taken. Please choose another.".to_string(),
                );
            }
        }
    }

    if feedback.is_empty() {
        log::info!("Successful claim: {}, {}", reg.name, reg.pdb);
        Ok(RegisterResponse::Success(Template::render("done", &*reg)))
    } else {
        feedback.insert("secret_key", SECRET_KEY.clone());
        feedback.insert("name", reg.name.clone());
        feedback.insert("pdb", reg.pdb.clone());
        feedback.insert("mcgill_id", reg.mcgill_id.clone());
        Ok(RegisterResponse::Conflict(Template::render(
            "index", feedback,
        )))
    }
}

#[derive(Responder, Debug)]
#[allow(clippy::large_enum_variant)]
enum AdminResponse {
    Ok(Template),

    #[response(status = 404)]
    NotFound(()),
}

async fn fetch_all(pool: &SqlitePool) -> Result<Vec<TemplateParams>> {
    Ok(sqlx::query_as("SELECT * FROM pdb_claims;")
        .fetch_all(pool)
        .await?)
}

#[get("/<secret_key>/admin")]
async fn admin(
    secret_key: SecretKey,
    pool: &State<SqlitePool>,
) -> Result<AdminResponse, InternalServerError> {
    match secret_key {
        SecretKey::User => Ok(AdminResponse::NotFound(())),
        SecretKey::Admin => {
            let mut template_ctx = HashMap::new();
            template_ctx.insert("registry", fetch_all(pool).await?);
            Ok(AdminResponse::Ok(Template::render("admin", &template_ctx)))
        }
    }
}

#[get("/<secret_key>/check")]
#[allow(unused_variables)]
async fn check(secret_key: SecretKey) -> Template {
    let mut params = HashMap::new();
    params.insert("mcgill_id", "");
    params.insert("secret_key", &SECRET_KEY);
    Template::render("check", params)
}

#[post("/<secret_key>/check", data = "<form>")]
#[allow(unused_variables)]
async fn do_check(
    secret_key: SecretKey,
    form: Form<CheckParams>,
    pool: &State<SqlitePool>,
) -> Result<RegisterResponse, InternalServerError> {
    if !MCG_ID_RE.is_match(&form.mcgill_id) {
        let mut feedback = HashMap::new();
        feedback.insert(
            "mcgill_id_validation_msg",
            "Please enter a valid 9-digit McGill ID.",
        );
        feedback.insert("mcgill_id", &form.mcgill_id);
        feedback.insert("secret_key", &SECRET_KEY);
        return Ok(RegisterResponse::Conflict(Template::render(
            "check", feedback,
        )));
    }
    let existing = already_registered(pool, &form.mcgill_id).await?;
    if let Some(pdb_id) = existing {
        let mut template_ctx = HashMap::new();
        template_ctx.insert("pdb", pdb_id);
        Ok(RegisterResponse::Success(Template::render(
            "done",
            template_ctx,
        )))
    } else {
        let mut feedback = HashMap::new();
        feedback.insert(
            "mcgill_id_validation_msg",
            "No PDB id registered for this McGill ID",
        );
        feedback.insert("mcgill_id", &form.mcgill_id);
        feedback.insert("secret_key", &SECRET_KEY);
        Ok(RegisterResponse::Conflict(Template::render(
            "check", feedback,
        )))
    }
}

#[get("/main.css")]
async fn css() -> std::result::Result<NamedFile, std::io::Error> {
    NamedFile::open("./main.css").await
}

#[get("/favicon.ico")]
async fn favicon() -> std::result::Result<NamedFile, std::io::Error> {
    NamedFile::open("./favicon.ico").await
}

async fn init_db(pool: &SqlitePool) -> Result<()> {
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS pdb_claims (
            pdb TEXT NOT NULL UNIQUE,
            mcgill_id TEXT NOT NULL PRIMARY KEY,
            name TEXT NOT NULL
        );
        "#,
    )
    .execute(pool)
    .await?;
    Ok(())
}

#[launch]
async fn rocket() -> _ {
    let opts = SqliteConnectOptions::from_str(&format!("sqlite://{DB_FILE}"))
        .unwrap()
        .create_if_missing(true)
        .synchronous(SqliteSynchronous::Full);
    let pool = sqlx::SqlitePool::connect_with(opts).await.unwrap();
    init_db(&pool).await.unwrap();
    rocket::build()
        .attach(Template::fairing())
        .mount(
            "/",
            routes![index, css, favicon, admin, register, check, do_check],
        )
        .manage(pool)
}
