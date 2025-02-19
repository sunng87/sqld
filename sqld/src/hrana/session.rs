use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{anyhow, bail, Context as _, Result};
use futures::future::BoxFuture;
use tokio::sync::{mpsc, oneshot};

use super::{proto, Server};
use crate::auth::{AuthError, Authenticated};
use crate::database::Database;
use crate::error::Error;
use crate::query::{Params, Query, QueryResponse, Value};
use crate::query_analysis::Statement;

/// Session-level state of an authenticated Hrana connection.
pub struct Session {
    _authenticated: Authenticated,
    streams: HashMap<i32, StreamHandle>,
}

struct StreamHandle {
    job_tx: mpsc::Sender<StreamJob>,
}

/// An arbitrary job that is executed on a [`Stream`].
///
/// All jobs are executed sequentially on a single task (as evidenced by the `&mut Stream` passed
/// to `f`).
struct StreamJob {
    /// The async function which performs the job.
    #[allow(clippy::type_complexity)]
    f: Box<dyn for<'s> FnOnce(&'s mut Stream) -> BoxFuture<'s, Result<proto::Response>> + Send>,
    /// The result of `f` will be sent here.
    resp_tx: oneshot::Sender<Result<proto::Response>>,
}

/// State of a Hrana stream, which corresponds to a standalone database connection.
struct Stream {
    /// The database handle is `None` when the stream is created, and normally set to `Some` by the
    /// first job executed on the stream by the [`proto::OpenStreamReq`] request. However, if that
    /// request returns an error, the following requests may encounter a `None` here.
    db: Option<Arc<dyn Database>>,
}

/// An error which can be converted to a Hrana [Error][proto::Error].
///
/// In the future, we may want to extend Hrana errors with a machine readable reason code, which
/// will correspond to a variant of this enum.
#[derive(thiserror::Error, Debug)]
pub enum ResponseError {
    #[error("Authentication failed: {source}")]
    Auth { source: AuthError },

    #[error("Stream {stream_id} not found")]
    StreamNotFound { stream_id: i32 },
    #[error("Stream {stream_id} already exists")]
    StreamExists { stream_id: i32 },
    #[error("Stream {stream_id} has failed to open")]
    StreamNotOpen { stream_id: i32 },

    #[error("SQL string does not contain any statement")]
    SqlNoStmt,
    #[error("SQL string contains more than one statement")]
    SqlManyStmts,
    #[error("Arguments do not match SQL parameters: {source}")]
    InvalidArgs { source: anyhow::Error },
    #[error("Transaction timed out")]
    TransactionTimeout,
    #[error("Server cannot handle additional transactions")]
    TransactionBusy,
    #[error("SQLite error: {source}: {message:?}")]
    SqliteError {
        source: rusqlite::ffi::Error,
        message: Option<String>,
    },
    #[error("SQL input error: {source}: {message:?} at offset {offset}")]
    SqlInputError {
        source: rusqlite::ffi::Error,
        message: String,
        offset: i32,
    },
}

pub(super) async fn handle_hello(server: &Server, jwt: Option<String>) -> Result<Session> {
    let _authenticated = server
        .auth
        .authenticate_jwt(jwt.as_deref())
        .map_err(|err| anyhow!(ResponseError::Auth { source: err }))?;

    Ok(Session {
        _authenticated,
        streams: HashMap::new(),
    })
}

pub(super) async fn handle_request(
    server: &Server,
    session: &mut Session,
    join_set: &mut tokio::task::JoinSet<()>,
    req: proto::Request,
) -> Result<oneshot::Receiver<Result<proto::Response>>> {
    let (resp_tx, resp_rx) = oneshot::channel();
    match req {
        proto::Request::OpenStream(req) => {
            let stream_id = req.stream_id;
            if session.streams.contains_key(&stream_id) {
                bail!(ResponseError::StreamExists { stream_id })
            }

            let mut stream_hnd = stream_spawn(join_set, Stream { db: None });

            let db_factory = server.db_factory.clone();
            stream_respond(&mut stream_hnd, resp_tx, move |stream| {
                Box::pin(async move {
                    let db = db_factory
                        .create()
                        .await
                        .context("Could not create a database connection")?;
                    stream.db = Some(db);
                    Ok(proto::Response::OpenStream(proto::OpenStreamResp {}))
                })
            })
            .await;

            session.streams.insert(stream_id, stream_hnd);
        }
        proto::Request::CloseStream(req) => {
            let stream_id = req.stream_id;
            let Some(mut stream_hnd) = session.streams.remove(&stream_id) else {
                bail!(ResponseError::StreamNotFound { stream_id })
            };

            stream_respond(&mut stream_hnd, resp_tx, |_| {
                Box::pin(async move { Ok(proto::Response::CloseStream(proto::CloseStreamResp {})) })
            })
            .await;
        }
        proto::Request::Execute(req) => {
            let stream_id = req.stream_id;
            let Some(stream_hnd) = session.streams.get_mut(&stream_id) else {
                bail!(ResponseError::StreamNotFound { stream_id })
            };

            stream_respond(stream_hnd, resp_tx, move |stream| {
                Box::pin(async move {
                    let Some(db) = stream.db.as_ref() else {
                        bail!(ResponseError::StreamNotOpen { stream_id })
                    };
                    let result = execute_stmt(&**db, req.stmt).await?;
                    Ok(proto::Response::Execute(proto::ExecuteResp { result }))
                })
            })
            .await;
        }
    }
    Ok(resp_rx)
}

fn stream_spawn(join_set: &mut tokio::task::JoinSet<()>, stream: Stream) -> StreamHandle {
    let (job_tx, mut job_rx) = mpsc::channel::<StreamJob>(8);
    join_set.spawn(async move {
        let mut stream = stream;
        while let Some(job) = job_rx.recv().await {
            let res = (job.f)(&mut stream).await;
            let _: Result<_, _> = job.resp_tx.send(res);
        }
    });
    StreamHandle { job_tx }
}

async fn stream_respond<F>(
    stream_hnd: &mut StreamHandle,
    resp_tx: oneshot::Sender<Result<proto::Response>>,
    f: F,
) where
    for<'s> F: FnOnce(&'s mut Stream) -> BoxFuture<'s, Result<proto::Response>>,
    F: Send + 'static,
{
    let job = StreamJob {
        f: Box::new(f),
        resp_tx,
    };
    let _: Result<_, _> = stream_hnd.job_tx.send(job).await;
}

async fn execute_stmt(db: &dyn Database, stmt: proto::Stmt) -> Result<proto::StmtResult> {
    let query = proto_stmt_to_query(stmt)?;
    let (query_result, _) = db.execute_one(query).await?;
    match query_result {
        Ok(query_response) => Ok(proto_stmt_result_from_query_response(query_response)),
        Err(error) => match ResponseError::try_from(error) {
            Ok(resp_error) => bail!(resp_error),
            Err(error) => bail!(error),
        },
    }
}

fn proto_stmt_to_query(proto_stmt: proto::Stmt) -> Result<Query> {
    let mut stmt_iter = Statement::parse(&proto_stmt.sql);
    let stmt = match stmt_iter.next() {
        Some(stmt_res) => stmt_res?,
        None => bail!(ResponseError::SqlNoStmt),
    };

    if stmt_iter.next().is_some() {
        bail!(ResponseError::SqlManyStmts)
    }

    let params = proto_stmt
        .args
        .into_iter()
        .map(proto_value_to_value)
        .collect();
    let params = Params::Positional(params);
    Ok(Query { stmt, params })
}

fn proto_stmt_result_from_query_response(query_response: QueryResponse) -> proto::StmtResult {
    let QueryResponse::ResultSet(result_set) = query_response;
    let proto_cols = result_set
        .columns
        .into_iter()
        .map(|col| proto::Col {
            name: Some(col.name),
        })
        .collect();
    let proto_rows = result_set
        .rows
        .into_iter()
        .map(|row| row.values.into_iter().map(proto::Value::from).collect())
        .collect();
    proto::StmtResult {
        cols: proto_cols,
        rows: proto_rows,
        affected_row_count: result_set.affected_row_count,
    }
}

fn proto_value_to_value(proto_value: proto::Value) -> Value {
    match proto_value {
        proto::Value::Null => Value::Null,
        proto::Value::Integer { value } => Value::Integer(value),
        proto::Value::Float { value } => Value::Real(value),
        proto::Value::Text { value } => Value::Text(value),
        proto::Value::Blob { value } => Value::Blob(value),
    }
}

fn proto_value_from_value(value: Value) -> proto::Value {
    match value {
        Value::Null => proto::Value::Null,
        Value::Integer(value) => proto::Value::Integer { value },
        Value::Real(value) => proto::Value::Float { value },
        Value::Text(value) => proto::Value::Text { value },
        Value::Blob(value) => proto::Value::Blob { value },
    }
}

fn proto_response_error_from_error(error: Error) -> Result<ResponseError, Error> {
    Ok(match error {
        Error::LibSqlInvalidQueryParams(source) => ResponseError::InvalidArgs { source },
        Error::LibSqlTxTimeout(_) => ResponseError::TransactionTimeout,
        Error::LibSqlTxBusy => ResponseError::TransactionBusy,
        Error::RusqliteError(rusqlite_error) => match rusqlite_error {
            rusqlite::Error::SqliteFailure(sqlite_error, message) => ResponseError::SqliteError {
                source: sqlite_error,
                message,
            },
            rusqlite::Error::SqlInputError {
                error: sqlite_error,
                msg: message,
                offset,
                ..
            } => ResponseError::SqlInputError {
                source: sqlite_error,
                message,
                offset,
            },
            rusqlite_error => return Err(Error::RusqliteError(rusqlite_error)),
        },
        error => return Err(error),
    })
}

impl From<proto::Value> for Value {
    fn from(proto_value: proto::Value) -> Value {
        proto_value_to_value(proto_value)
    }
}

impl From<Value> for proto::Value {
    fn from(value: Value) -> proto::Value {
        proto_value_from_value(value)
    }
}

impl TryFrom<Error> for ResponseError {
    type Error = Error;
    fn try_from(error: Error) -> Result<ResponseError, Error> {
        proto_response_error_from_error(error)
    }
}
