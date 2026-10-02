package store

import "sync"

type Cache struct {
	mu    sync.Mutex
	items map[string]string
}

// Get is a cache lookup, not a route.
func (c *Cache) Get(key string) string {
	c.mu.Lock()
	defer c.mu.Unlock()
	return c.items[key]
}

func Use(c *Cache) string {
	return c.Get("/not/a/route")
}
