use rig::embeddings::{DocumentEmbeddings, Embedding, EmbeddingModel};
use rig::vector_store::{VectorStore, VectorStoreError, VectorStoreIndex};
use rusqlite::ffi::sqlite3_auto_extension;
use rusqlite::OptionalExtension;
use serde::Deserialize;
use sqlite_vec::sqlite3_vec_init;
use std::path::Path;
use tokio_rusqlite::Connection;
use tracing::{debug, info};
use zerocopy::IntoBytes;

#[derive(Debug, Deserialize)]
pub struct Account {
    pub id: i64,
    pub name: String,
    pub source: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Deserialize)]
pub struct Conversation {
    pub id: i64,
    pub user_id: i64,
    pub title: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Deserialize)]
pub struct Message {
    pub id: i64,
    pub channel_id: i64,
    pub account_id: i64,
    pub role: String,
    pub content: String,
    pub reply_to_id: Option<i64>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Deserialize)]
pub struct Channel {
    pub id: i64,
    pub name: String,
    pub source: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug)]
pub enum SqliteError {
    DatabaseError(Box<dyn std::error::Error + Send + Sync>),
    SerializationError(Box<dyn std::error::Error + Send + Sync>),
}

#[derive(Clone)]
pub struct SqliteStore {
    pub conn: Connection,
}

impl SqliteStore {
    pub async fn new<P: AsRef<Path>>(path: P) -> Result<Self, VectorStoreError> {
        info!("Initializing SQLite store at {:?}", path.as_ref());
        unsafe {
            sqlite3_auto_extension(Some(std::mem::transmute(sqlite3_vec_init as *const ())));
        }

        let conn = Connection::open(path)
            .await
            .map_err(|e| VectorStoreError::DatastoreError(Box::new(e)))?;

        debug!("Running initial migrations");
        // Run migrations or create tables if they don't exist
        conn.call(|conn| {
            conn.execute_batch(
                "BEGIN;
                -- Document tables
                CREATE TABLE IF NOT EXISTS documents (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    doc_id TEXT UNIQUE NOT NULL,
                    document TEXT NOT NULL
                );
                CREATE INDEX IF NOT EXISTS idx_doc_id ON documents(doc_id);
                CREATE VIRTUAL TABLE IF NOT EXISTS embeddings USING vec0(embedding float[1536]);

                -- User management tables
                CREATE TABLE IF NOT EXISTS accounts (
                    id INTEGER PRIMARY KEY,
                    name TEXT NOT NULL UNIQUE,
                    source TEXT NOT NULL,
                    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
                    updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
	                );

                -- Channel tables
                CREATE TABLE IF NOT EXISTS channels (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    channel_id TEXT NOT NULL UNIQUE,
                    channel_type TEXT NOT NULL, -- 'discord', 'twitter', 'telegram' etc
                    name TEXT,
                    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
                    updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
                );
                CREATE INDEX IF NOT EXISTS idx_channel_id_type ON channels(channel_id, channel_type);

                -- Channel membership table
                CREATE TABLE IF NOT EXISTS channel_members (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    channel_id INTEGER NOT NULL,
                    account_id INTEGER NOT NULL,
                    joined_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
                    FOREIGN KEY (channel_id) REFERENCES channels(id),
                    FOREIGN KEY (account_id) REFERENCES accounts(id)
                );
                CREATE INDEX IF NOT EXISTS idx_channel_members ON channel_members(channel_id, account_id);

                -- Messages table
                CREATE TABLE IF NOT EXISTS messages (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    channel_id INTEGER NOT NULL,
                    account_id INTEGER NOT NULL,
                    content TEXT NOT NULL,
					role TEXT NOT NULL,
					reply_to_id INTEGER,
                    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
                    FOREIGN KEY (channel_id) REFERENCES channels(id),
                    FOREIGN KEY (account_id) REFERENCES accounts(id)
                );
                CREATE INDEX IF NOT EXISTS idx_messages_channel ON messages(channel_id);
                CREATE INDEX IF NOT EXISTS idx_messages_account ON messages(account_id);

                COMMIT;",
            )
            .map_err(|e| tokio_rusqlite::Error::from(e))
        })
        .await
        .map_err(|e| VectorStoreError::DatastoreError(Box::new(e)))?;

        Ok(Self { conn })
    }

    fn serialize_embedding(embedding: &Embedding) -> Vec<f32> {
        embedding.vec.iter().map(|x| *x as f32).collect()
    }

    pub async fn create_user(
        &self,
        name: String,
        source: String,
        account_id: i64,
    ) -> Result<i64, SqliteError> {
        self.conn
            .call(move |conn| {
                conn.query_row(
                    "INSERT INTO accounts (id, name, source, created_at, updated_at)
                 VALUES (?1, ?2, ?3, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP)
                 ON CONFLICT(name) DO UPDATE SET 
                     updated_at = CURRENT_TIMESTAMP
                 RETURNING id",
                    rusqlite::params![account_id, name, source],
                    |row| row.get(0),
                )
                .map_err(|e| tokio_rusqlite::Error::from(e))
            })
            .await
            .map_err(|e| SqliteError::DatabaseError(Box::new(e)))
    }
    pub async fn get_user_by_source(&self, source: String) -> Result<Option<Account>, SqliteError> {
        self.conn
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT id, name, source, created_at, updated_at FROM accounts WHERE source = ?1"
                )?;

                let account = stmt.query_row(rusqlite::params![source], |row| {
                    Ok(Account {
                        id: row.get(0)?,
                        name: row.get(1)?,
                        source: row.get(2)?,
                        created_at: row.get::<_, String>(3)?.parse().unwrap(),
                        updated_at: row.get::<_, String>(4)?.parse().unwrap(),
                    })
                }).optional()?;

                Ok(account)
            })
            .await
            .map_err(|e| SqliteError::DatabaseError(Box::new(e)))
    }

    pub async fn create_channel(
        &self,
        channel_id: String,
        channel_type: String,
        name: Option<String>,
    ) -> Result<i64, SqliteError> {
        self.conn
            .call(move |conn| {
                conn.query_row(
                    "INSERT INTO channels (channel_id, channel_type, name, created_at, updated_at)
                 VALUES (?1, ?2, ?3, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP)
                 ON CONFLICT(channel_id) DO UPDATE SET 
                     name = COALESCE(?3, name),
                     updated_at = CURRENT_TIMESTAMP
                 RETURNING id",
                    rusqlite::params![channel_id, channel_type, name],
                    |row| row.get(0),
                )
                .map_err(|e| tokio_rusqlite::Error::from(e))
            })
            .await
            .map_err(|e| SqliteError::DatabaseError(Box::new(e)))
    }
    pub async fn get_channel(&self, id: i64) -> Result<Option<Channel>, SqliteError> {
        self.conn
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT id, name, source, created_at, updated_at FROM channels WHERE id = ?1",
                )?;

                let channel = stmt
                    .query_row(rusqlite::params![id], |row| {
                        Ok(Channel {
                            id: row.get(0)?,
                            name: row.get(1)?,
                            source: row.get(2)?,
                            created_at: row.get::<_, String>(3)?.parse().unwrap(),
                            updated_at: row.get::<_, String>(4)?.parse().unwrap(),
                        })
                    })
                    .optional()?;

                Ok(channel)
            })
            .await
            .map_err(|e| SqliteError::DatabaseError(Box::new(e)))
    }

    pub async fn get_channels_by_source(
        &self,
        source: String,
    ) -> Result<Vec<Channel>, SqliteError> {
        self.conn
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT id, name, source, created_at, updated_at FROM channels WHERE source = ?1"
                )?;

                let channels = stmt.query_map(rusqlite::params![source], |row| {
                    Ok(Channel {
                        id: row.get(0)?,
                        name: row.get(1)?,
                        source: row.get(2)?,
                        created_at: row.get::<_, String>(3)?.parse().unwrap(),
                        updated_at: row.get::<_, String>(4)?.parse().unwrap(),
                    })
                }).and_then(|mapped_rows| {
                    mapped_rows.collect::<Result<Vec<Channel>, _>>()
                })?;

                Ok(channels)
            })
            .await
            .map_err(|e| SqliteError::DatabaseError(Box::new(e)))
    }

    pub async fn create_conversation(
        &self,
        user_id: i64,
        title: String,
    ) -> Result<i64, SqliteError> {
        self.conn
            .call(move |conn| {
                let tx = conn.transaction()?;

                let id = tx.query_row(
                    "INSERT INTO conversations (user_id, title) VALUES (?1, ?2) RETURNING id",
                    rusqlite::params![user_id, title],
                    |row| row.get(0),
                )?;

                tx.commit()?;

                Ok(id)
            })
            .await
            .map_err(|e| SqliteError::DatabaseError(Box::new(e)))
    }

    pub async fn get_conversation(&self, id: i64) -> Result<Option<Conversation>, SqliteError> {
        self.conn
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT id, user_id, title, created_at, updated_at FROM conversations WHERE id = ?1"
                )?;

                let conversation = stmt.query_row(rusqlite::params![id], |row| {
                    Ok(Conversation {
                        id: row.get(0)?,
                        user_id: row.get(1)?,
                        title: row.get(2)?,
                        created_at: row.get::<_, String>(3)?.parse().unwrap(),
                        updated_at: row.get::<_, String>(4)?.parse().unwrap(),
                    })
                }).optional()?;

                Ok(conversation)
            })
            .await
            .map_err(|e| SqliteError::DatabaseError(Box::new(e)))
    }

    pub async fn get_conversations_by_user(
        &self,
        user_id: i64,
    ) -> Result<Vec<Conversation>, SqliteError> {
        self.conn
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT id, user_id, title, created_at, updated_at FROM conversations WHERE user_id = ?1"
                )?;

                let conversations = stmt.query_map(rusqlite::params![user_id], |row| {
                    Ok(Conversation {
                        id: row.get(0)?,
                        user_id: row.get(1)?,
                        title: row.get(2)?,
                        created_at: row.get::<_, String>(3)?.parse().unwrap(),
                        updated_at: row.get::<_, String>(4)?.parse().unwrap(),
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;

                Ok(conversations)
            })
            .await
            .map_err(|e| SqliteError::DatabaseError(Box::new(e)))
    }

    pub async fn create_message(
        &self,
        channel_id: i64,
        account_id: i64,
        reply_to_id: Option<i64>,
        role: String,
        content: String,
    ) -> Result<i64, SqliteError> {
        self.conn
            .call(move |conn| {
                let tx = conn.transaction()?;

                let id = tx.query_row(
					"INSERT INTO messages (channel_id, account_id, content, role, reply_to_id) VALUES (?1, ?2, ?3, ?4, ?5) RETURNING id",
					rusqlite::params![channel_id, account_id, content, role, reply_to_id],
					|row| row.get(0)
				)?;

                tx.commit()?;

                Ok(id)
            })
            .await
            .map_err(|e| SqliteError::DatabaseError(Box::new(e)))
    }

    pub async fn get_message(&self, id: i64) -> Result<Option<Message>, SqliteError> {
        self.conn
			.call(move |conn| {
				Ok(conn.prepare("SELECT id, channel_id, account_id, role, content, reply_to_id, created_at FROM messages WHERE id = ?1")?
					.query_row(rusqlite::params![id], |row| {
						let created_at_str: String = row.get(6)?;
						tracing::info!("created_at_str: {}", created_at_str);
                    let created_at = chrono::NaiveDateTime::parse_from_str(&created_at_str, "%Y-%m-%d %H:%M:%S")
                        .map_err(|_| rusqlite::Error::InvalidQuery)?
                        .and_utc();
						Ok(Message {
							id: row.get(0)?,
							channel_id: row.get(1)?,
							account_id: row.get(2)?,
							role: row.get(3)?,
							content: row.get(4)?,
							reply_to_id: row.get(5)?,
							created_at,
						})
					}).optional().unwrap())
			})
			.await
			.map_err(|e| SqliteError::DatabaseError(Box::new(e)))
    }

    pub async fn get_conversation_between_users(
        &self,
        user_id: i64,
        other_user_id: i64,
        since: chrono::DateTime<chrono::Utc>,
        limit: usize,
    ) -> Result<Vec<Message>, SqliteError> {
        self.conn
            .call(move |conn| {
                let query =
                    "SELECT m.id, m.channel_id, m.account_id, m.role, m.content, m.reply_to_id, m.created_at 
                FROM messages m 
                WHERE (m.account_id = ?1 OR (m.reply_to_id = ?1 AND m.account_id = ?2)) 
                    AND m.created_at > ?3
                ORDER BY m.created_at ASC
                LIMIT ?4";

            let mut stmt = conn.prepare(query)?;

            let messages = stmt.query_map(
                rusqlite::params![
                            user_id,
                            other_user_id,
                            since.format("%Y-%m-%d %H:%M:%S").to_string(),
                            limit
                        ],
                        |row| {
                            let created_at_str: String = row.get(6)?;
                            let created_at = chrono::NaiveDateTime::parse_from_str(
                                &created_at_str,
                                "%Y-%m-%d %H:%M:%S",
                            )
                            .map_err(|_| rusqlite::Error::InvalidQuery)?
                            .and_utc();

                            Ok(Message {
                                id: row.get(0)?,
                                channel_id: row.get(1)?,
                                account_id: row.get(2)?,
                                role: row.get(3)?,
                                content: row.get(4)?,
                                reply_to_id: row.get(5)?,
                                created_at,
                            })
                        },
                    )?
                    .collect::<Result<Vec<Message>, _>>()?;

                Ok(messages)
            })
            .await
            .map_err(|e| SqliteError::DatabaseError(Box::new(e)))
    }

    pub async fn get_recent_messages(
        &self,
        channel_id: i64,
        limit: usize,
    ) -> Result<Vec<Message>, SqliteError> {
        self.conn
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT id, channel_id, account_id, role, content, reply_to_id, created_at 
                     FROM messages 
                     WHERE channel_id = ?1 
                     ORDER BY created_at DESC 
                     LIMIT ?2",
                )?;

                let messages = stmt
                    .query_map(rusqlite::params![channel_id, limit], |row| {
                        let created_at_str: String = row.get(6)?;
                        let created_at = chrono::NaiveDateTime::parse_from_str(
                            &created_at_str,
                            "%Y-%m-%d %H:%M:%S",
                        )
                        .map_err(|_| rusqlite::Error::InvalidQuery)?
                        .and_utc();

                        Ok(Message {
                            id: row.get(0)?,
                            channel_id: row.get(1)?,
                            account_id: row.get(2)?,
                            role: row.get(3)?,
                            content: row.get(4)?,
                            reply_to_id: row.get(5)?,
                            created_at,
                        })
                    })?
                    .collect::<Result<Vec<_>, _>>()?;

                Ok(messages)
            })
            .await
            .map_err(|e| SqliteError::DatabaseError(Box::new(e)))
    }

    pub async fn get_messages_by_channel(
        &self,
        channel_id: i64,
    ) -> Result<Vec<Message>, SqliteError> {
        self.conn
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT id, channel_id, account_id, role, content, reply_to_id, created_at FROM messages WHERE channel_id = ?1 ORDER BY created_at ASC"
                )?;

                let messages = stmt.query_map(rusqlite::params![channel_id], |row| {
                    Ok(Message {
                        id: row.get(0)?,
                        channel_id: row.get(1)?,
                        account_id: row.get(2)?,
                        role: row.get(3)?,
                        content: row.get(4)?,
                        reply_to_id: row.get(5)?,
                        created_at: row.get::<_, String>(6)?.parse().unwrap(),
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;

                Ok(messages)
            })
            .await
            .map_err(|e| SqliteError::DatabaseError(Box::new(e)))
    }
}

impl VectorStore for SqliteStore {
    type Q = String;

    async fn add_documents(
        &mut self,
        documents: Vec<DocumentEmbeddings>,
    ) -> Result<(), VectorStoreError> {
        info!("Adding {} documents to store", documents.len());
        self.conn
            .call(|conn| {
                let tx = conn
                    .transaction()
                    .map_err(|e| tokio_rusqlite::Error::from(e))?;

                for doc in documents {
                    debug!("Storing document with id {}", doc.id);
                    // Store document and get auto-incremented ID
                    tx.execute(
                        "INSERT OR REPLACE INTO documents (doc_id, document) VALUES (?1, ?2)",
                        &[&doc.id, &doc.document.to_string()],
                    )
                    .map_err(|e| tokio_rusqlite::Error::from(e))?;

                    let doc_id = tx.last_insert_rowid();

                    // Store embeddings
                    let mut stmt = tx
                        .prepare("INSERT INTO embeddings (rowid, embedding) VALUES (?1, ?2)")
                        .map_err(|e| tokio_rusqlite::Error::from(e))?;

                    debug!(
                        "Storing {} embeddings for document {}",
                        doc.embeddings.len(),
                        doc.id
                    );
                    for embedding in doc.embeddings {
                        let vec = Self::serialize_embedding(&embedding);
                        let blob = rusqlite::types::Value::Blob(vec.as_slice().as_bytes().to_vec());
                        stmt.execute(rusqlite::params![doc_id, blob])
                            .map_err(|e| tokio_rusqlite::Error::from(e))?;
                    }
                }

                tx.commit().map_err(|e| tokio_rusqlite::Error::from(e))?;
                Ok(())
            })
            .await
            .map_err(|e| VectorStoreError::DatastoreError(Box::new(e)))?;

        Ok(())
    }

    async fn get_document<T: for<'a> Deserialize<'a>>(
        &self,
        id: &str,
    ) -> Result<Option<T>, VectorStoreError> {
        debug!("Fetching document with id {}", id);
        let id_clone = id.to_string();
        let doc_str = self
            .conn
            .call(move |conn| {
                conn.query_row(
                    "SELECT document FROM documents WHERE doc_id = ?1",
                    rusqlite::params![id_clone],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(|e| tokio_rusqlite::Error::from(e))
            })
            .await
            .map_err(|e| VectorStoreError::DatastoreError(Box::new(e)))?;

        match doc_str {
            Some(doc_str) => {
                let doc: T = serde_json::from_str(&doc_str)
                    .map_err(|e| VectorStoreError::DatastoreError(Box::new(e)))?;
                Ok(Some(doc))
            }
            None => {
                debug!("No document found with id {}", id);
                Ok(None)
            }
        }
    }

    async fn get_document_embeddings(
        &self,
        id: &str,
    ) -> Result<Option<DocumentEmbeddings>, VectorStoreError> {
        debug!("Fetching embeddings for document {}", id);
        // First get the document
        let doc: Option<serde_json::Value> = self.get_document(&id).await?;

        if let Some(doc) = doc {
            let id_clone = id.to_string();
            let embeddings = self
                .conn
                .call(move |conn| {
                    let mut stmt = conn.prepare(
                        "SELECT e.embedding 
                         FROM embeddings e
                         JOIN documents d ON e.rowid = d.id
                         WHERE d.doc_id = ?1",
                    )?;

                    let embeddings = stmt
                        .query_map(rusqlite::params![id_clone], |row| {
                            let bytes: Vec<u8> = row.get(0)?;
                            let vec = bytes
                                .chunks(4)
                                .map(|chunk| f32::from_le_bytes(chunk.try_into().unwrap()) as f64)
                                .collect();
                            Ok(rig::embeddings::Embedding {
                                vec,
                                document: "".to_string(),
                            })
                        })?
                        .collect::<Result<Vec<_>, _>>()?;
                    Ok(embeddings)
                })
                .await
                .map_err(|e| VectorStoreError::DatastoreError(Box::new(e)))?;

            debug!("Found {} embeddings for document {}", embeddings.len(), id);
            Ok(Some(DocumentEmbeddings {
                id: id.to_string(),
                document: doc,
                embeddings,
            }))
        } else {
            debug!("No embeddings found for document {}", id);
            Ok(None)
        }
    }

    async fn get_document_by_query(
        &self,
        query: Self::Q,
    ) -> Result<Option<DocumentEmbeddings>, VectorStoreError> {
        debug!("Searching for document matching query");
        let result = self
            .conn
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT d.doc_id, e.distance 
                     FROM embeddings e
                     JOIN documents d ON e.rowid = d.id
                     WHERE e.embedding MATCH ?1  AND k = ?2
                     ORDER BY e.distance",
                )?;

                let result = stmt
                    .query_row(rusqlite::params![query.as_bytes(), 1], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?))
                    })
                    .optional()?;
                Ok(result)
            })
            .await
            .map_err(|e| VectorStoreError::DatastoreError(Box::new(e)))?;

        match result {
            Some((id, distance)) => {
                debug!("Found matching document {} with distance {}", id, distance);
                self.get_document_embeddings(&id).await
            }
            None => {
                debug!("No matching documents found");
                Ok(None)
            }
        }
    }
}

pub struct SqliteVectorIndex<E: EmbeddingModel> {
    store: SqliteStore,
    embedding_model: E,
}

impl<E: EmbeddingModel> SqliteVectorIndex<E> {
    pub fn new(embedding_model: E, store: SqliteStore) -> Self {
        Self {
            store,
            embedding_model,
        }
    }
}

impl<E: EmbeddingModel + std::marker::Sync> VectorStoreIndex for SqliteVectorIndex<E> {
    async fn top_n<T: for<'a> Deserialize<'a>>(
        &self,
        query: &str,
        n: usize,
    ) -> Result<Vec<(f64, String, T)>, VectorStoreError> {
        debug!("Finding top {} matches for query", n);
        let embedding = self.embedding_model.embed_document(query).await?;
        let query_vec = SqliteStore::serialize_embedding(&embedding);

        let rows = self
            .store
            .conn
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT d.doc_id, e.distance 
                    FROM embeddings e
                    JOIN documents d ON e.rowid = d.id
                    WHERE e.embedding MATCH ?1 AND k = ?2
                    ORDER BY e.distance",
                )?;

                let rows = stmt
                    .query_map(rusqlite::params![query_vec.as_bytes(), n], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?))
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(rows)
            })
            .await
            .map_err(|e| VectorStoreError::DatastoreError(Box::new(e)))?;

        debug!("Found {} potential matches", rows.len());
        let mut top_n = Vec::new();
        for (id, distance) in rows {
            if let Some(doc) = self.store.get_document(&id).await? {
                top_n.push((distance, id, doc));
            }
        }

        debug!("Returning {} matches", top_n.len());
        Ok(top_n)
    }

    async fn top_n_ids(
        &self,
        query: &str,
        n: usize,
    ) -> Result<Vec<(f64, String)>, VectorStoreError> {
        debug!("Finding top {} document IDs for query", n);
        let embedding = self.embedding_model.embed_document(query).await?;
        let query_vec = SqliteStore::serialize_embedding(&embedding);

        let results = self
            .store
            .conn
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT d.doc_id, e.distance 
                     FROM embeddings e
                     JOIN documents d ON e.rowid = d.id
                     WHERE e.embedding MATCH ?1  AND k = ?2
                     ORDER BY e.distance",
                )?;

                let results = stmt
                    .query_map(rusqlite::params![query_vec.as_bytes(), n], |row| {
                        Ok((row.get::<_, f64>(1)?, row.get::<_, String>(0)?))
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(results)
            })
            .await
            .map_err(|e| VectorStoreError::DatastoreError(Box::new(e)))?;

        debug!("Found {} matching document IDs", results.len());
        Ok(results)
    }
}
