package cache

import (
	"context"

	"github.com/redis/go-redis/v9"
)

func Wire(ctx context.Context, rdb *redis.Client) {
	rdb.Publish(ctx, "shelf.updated", "1")
	sub := rdb.Subscribe(ctx, "shelf.cleared")
	defer sub.Close()
}
