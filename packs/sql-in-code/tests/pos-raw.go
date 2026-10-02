package store

import "context"

func (r *Repo) Load(ctx context.Context, id string) error {
	row := r.db.QueryRow(ctx, `SELECT b.id, b.name FROM bays b JOIN zones z ON z.id = b.zone_id WHERE b.id = $1`, id)
	return row.Scan()
}

func (r *Repo) Rename(ctx context.Context, id, name string) error {
	_, err := r.db.Exec(ctx, "UPDATE bays SET name = $2 WHERE id = $1", id, name)
	return err
}
