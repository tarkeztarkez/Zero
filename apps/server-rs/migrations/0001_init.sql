CREATE TABLE users (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    email TEXT NOT NULL UNIQUE,
    image TEXT,
    default_connection_id TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE sessions (
    id TEXT PRIMARY KEY,
    token TEXT NOT NULL UNIQUE,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    expires_at TIMESTAMPTZ NOT NULL,
    ip_address TEXT,
    user_agent TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Pending OAuth flows (sign-in or linking another mailbox).
CREATE TABLE oauth_states (
    state TEXT PRIMARY KEY,
    code_verifier TEXT NOT NULL,
    callback_url TEXT NOT NULL,
    link_user_id TEXT REFERENCES users(id) ON DELETE CASCADE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- A mailbox. provider_id is 'google' or 'imap'.
-- Secrets (refresh token, IMAP password) are encrypted with the server key.
CREATE TABLE connections (
    id TEXT PRIMARY KEY,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    email TEXT NOT NULL,
    name TEXT,
    picture TEXT,
    provider_id TEXT NOT NULL,
    access_token TEXT,
    refresh_token TEXT,
    expires_at TIMESTAMPTZ,
    scope TEXT NOT NULL DEFAULT '',
    imap_config JSONB,
    secret TEXT,
    sync_state JSONB NOT NULL DEFAULT '{}'::jsonb,
    last_sync_at TIMESTAMPTZ,
    last_sync_error TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (user_id, email)
);

CREATE TABLE labels (
    connection_id TEXT NOT NULL REFERENCES connections(id) ON DELETE CASCADE,
    id TEXT NOT NULL,
    name TEXT NOT NULL,
    type TEXT NOT NULL DEFAULT 'user',
    color JSONB,
    PRIMARY KEY (connection_id, id)
);

CREATE TABLE threads (
    connection_id TEXT NOT NULL REFERENCES connections(id) ON DELETE CASCADE,
    id TEXT NOT NULL,
    latest_received_on TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (connection_id, id)
);
CREATE INDEX threads_latest_idx ON threads (connection_id, latest_received_on DESC);

CREATE TABLE thread_labels (
    connection_id TEXT NOT NULL,
    thread_id TEXT NOT NULL,
    label_id TEXT NOT NULL,
    PRIMARY KEY (connection_id, thread_id, label_id),
    FOREIGN KEY (connection_id, thread_id) REFERENCES threads(connection_id, id) ON DELETE CASCADE
);
CREATE INDEX thread_labels_label_idx ON thread_labels (connection_id, label_id);

-- One row per message. data holds the ParsedMessage JSON the frontend renders.
-- provider_ref locates the message upstream (Gmail id, or IMAP folder/uid).
CREATE TABLE messages (
    connection_id TEXT NOT NULL,
    id TEXT NOT NULL,
    thread_id TEXT NOT NULL,
    message_id_header TEXT,
    received_on TIMESTAMPTZ NOT NULL,
    label_ids TEXT[] NOT NULL DEFAULT '{}',
    provider_ref JSONB NOT NULL DEFAULT '{}'::jsonb,
    data JSONB NOT NULL,
    search_text TEXT NOT NULL DEFAULT '',
    PRIMARY KEY (connection_id, id),
    FOREIGN KEY (connection_id, thread_id) REFERENCES threads(connection_id, id) ON DELETE CASCADE
);
CREATE INDEX messages_thread_idx ON messages (connection_id, thread_id, received_on);
CREATE INDEX messages_msgid_idx ON messages (connection_id, message_id_header);
CREATE INDEX messages_search_idx ON messages USING gin (to_tsvector('simple', search_text));

CREATE TABLE user_settings (
    user_id TEXT PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    settings JSONB NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE notes (
    id TEXT PRIMARY KEY,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    thread_id TEXT NOT NULL,
    content TEXT NOT NULL,
    color TEXT NOT NULL DEFAULT 'default',
    is_pinned BOOLEAN NOT NULL DEFAULT false,
    "order" INTEGER NOT NULL DEFAULT 0,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX notes_user_thread_idx ON notes (user_id, thread_id);

CREATE TABLE email_templates (
    id TEXT PRIMARY KEY,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    subject TEXT,
    body TEXT,
    "to" JSONB,
    cc JSONB,
    bcc JSONB,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (user_id, name)
);

-- Drafts for IMAP mailboxes (Gmail drafts live in Gmail).
CREATE TABLE local_drafts (
    id TEXT PRIMARY KEY,
    connection_id TEXT NOT NULL REFERENCES connections(id) ON DELETE CASCADE,
    thread_id TEXT,
    data JSONB NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Emails waiting to be sent (undo-send and scheduled send).
CREATE TABLE outbox (
    id TEXT PRIMARY KEY,
    connection_id TEXT NOT NULL REFERENCES connections(id) ON DELETE CASCADE,
    payload JSONB NOT NULL,
    send_at TIMESTAMPTZ NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending',
    error TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX outbox_due_idx ON outbox (status, send_at);

-- Snoozed threads wake up at wake_at and return to the inbox.
CREATE TABLE snoozes (
    connection_id TEXT NOT NULL REFERENCES connections(id) ON DELETE CASCADE,
    thread_id TEXT NOT NULL,
    wake_at TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (connection_id, thread_id)
);
