package fleet

type Cache struct{}

func (c *Cache) GetRoute(id string) string {
	return id
}

func Use(c *Cache) string {
	return c.GetRoute("r1")
}
