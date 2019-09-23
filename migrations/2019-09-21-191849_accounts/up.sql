CREATE TABLE accounts (
  address bytea NOT NULL PRIMARY KEY,
  -- a constrained biginteger type whose max value corresponds with that of a u32 in rust
  free_balance bigint check (free_balance >= 0 and free_balance < '4294967296'::bigint) NOT NULL,
  -- a constrained biginteger type whose max value corresponds with that of a u32 in rust
  reserved_balance bigint check (reserved_balance >= 0 and reserved_balance < '4294967296'::bigint) NOT NULL,
  account_index bytea NOT NULL,
  nonce int check (nonce >= 0) NOT NULL,
  -- hash of block that the account was created in
  create_hash bytea NOT NULL REFERENCES blocks(hash) ON DELETE RESTRICT,
  -- a constrained biginteger type whose max value corresponds with that of a u32 in rust
  created bigint check (created >= 0 and created < '4294967296'::bigint) NOT NULL,
  -- a constrained biginteger type whose max value corresponds with that of a u32 in rust
  updated bigint check (updated >= 0 and created < '4294967296'::bigint) NOT NULL,
  active bool NOT NULL
);

