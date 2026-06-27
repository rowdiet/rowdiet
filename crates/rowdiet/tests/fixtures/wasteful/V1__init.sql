CREATE TABLE account (
    active boolean NOT NULL,
    id bigint PRIMARY KEY,
    kind smallint NOT NULL,
    balance bigint NOT NULL
);
