CREATE TABLE bays (
    id         uuid PRIMARY KEY,
    name       varchar(64) NOT NULL,
    capacity   integer NOT NULL DEFAULT 0,
    created_at timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX bays_name_idx ON bays (name);
