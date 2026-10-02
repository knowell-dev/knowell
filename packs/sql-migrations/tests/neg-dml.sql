-- Data changes only: no schema definitions.
INSERT INTO bays (id, name) VALUES ('b1', 'North');
UPDATE bays SET name = 'South' WHERE id = 'b1';
SELECT id, name FROM bays;
