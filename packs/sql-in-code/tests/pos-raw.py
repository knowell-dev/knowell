def archive(cursor, bay_id):
    cursor.execute("INSERT INTO bay_archive (id) SELECT id FROM bays WHERE id = %s", (bay_id,))
    cursor.execute("DELETE FROM bays WHERE id = %s", (bay_id,))
