use anyhow::Result;
use rusqlite::Connection;
use std::sync::Mutex;

pub struct Database {
    conn: Mutex<Connection>,
}

impl Database {
    pub fn open(path: &str) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn migrate(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS projects (
                id TEXT PRIMARY KEY,
                owner TEXT NOT NULL,
                repo TEXT NOT NULL,
                default_branch TEXT NOT NULL DEFAULT 'main',
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                config TEXT NOT NULL DEFAULT '{}',
                skills TEXT NOT NULL DEFAULT ''
            );

            CREATE TABLE IF NOT EXISTS tasks (
                id TEXT PRIMARY KEY,
                project_id TEXT NOT NULL REFERENCES projects(id),
                issue_number INTEGER,
                title TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'pending',
                agent_type TEXT NOT NULL DEFAULT 'codex',
                model TEXT NOT NULL DEFAULT 'claude-sonnet-4.5',
                worktree_path TEXT,
                branch TEXT,
                pid INTEGER,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                finished_at TEXT,
                auto_merge INTEGER NOT NULL DEFAULT 0
            );

            CREATE TABLE IF NOT EXISTS task_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                task_id TEXT NOT NULL REFERENCES tasks(id),
                kind TEXT NOT NULL,
                icon TEXT NOT NULL DEFAULT '📝',
                message TEXT NOT NULL,
                detail TEXT,
                resolved TEXT,
                created_at TEXT NOT NULL DEFAULT (datetime('now'))
            );

            -- Legacy tables kept for migration compat
            CREATE TABLE IF NOT EXISTS agents (
                id TEXT PRIMARY KEY,
                project_id TEXT NOT NULL,
                issue_number INTEGER,
                status TEXT NOT NULL DEFAULT 'pending',
                model TEXT NOT NULL,
                worktree_path TEXT,
                branch TEXT,
                pid INTEGER,
                prompt TEXT NOT NULL,
                output TEXT NOT NULL DEFAULT '',
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                finished_at TEXT,
                exit_code INTEGER
            );

            CREATE TABLE IF NOT EXISTS quality_gates (
                id TEXT PRIMARY KEY,
                agent_id TEXT NOT NULL,
                gate_type TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'pending',
                output TEXT NOT NULL DEFAULT '',
                created_at TEXT NOT NULL DEFAULT (datetime('now'))
            );

            CREATE TABLE IF NOT EXISTS chat_messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                project_id TEXT,
                role TEXT NOT NULL,
                content TEXT NOT NULL,
                actions TEXT,
                created_at TEXT NOT NULL DEFAULT (datetime('now'))
            );

            CREATE INDEX IF NOT EXISTS idx_tasks_project ON tasks(project_id);
            CREATE INDEX IF NOT EXISTS idx_tasks_status ON tasks(status);
            CREATE INDEX IF NOT EXISTS idx_events_task ON task_events(task_id);
            CREATE INDEX IF NOT EXISTS idx_chat_project ON chat_messages(project_id, id DESC);
            ",
        )?;
        Ok(())
    }

    pub fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap()
    }
}
