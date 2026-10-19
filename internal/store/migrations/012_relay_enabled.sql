-- The owner's relay switch from the web UI's status page: 1 for on, 0 for
-- off, NULL when the owner has not set it. NULL means the config key relay
-- applies. A set value overrides the config key. The value lives on the
-- relay_settings row; a toggle before the first call of GET /servers/:id
-- creates the row with an empty connect_url, and the store treats a row with
-- an empty connect_url as no stored settings.
ALTER TABLE relay_settings ADD COLUMN enabled INTEGER NULL;
