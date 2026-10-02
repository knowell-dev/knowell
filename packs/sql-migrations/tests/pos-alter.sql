-- Add a zone and retire the old capacity column.
ALTER TABLE bays ADD COLUMN zone varchar(16);
ALTER TABLE bays DROP COLUMN capacity;
ALTER TABLE public.bays
    ADD COLUMN door varchar(8),
    ADD COLUMN heated boolean NOT NULL DEFAULT false;
DROP TABLE IF EXISTS legacy_bays;
