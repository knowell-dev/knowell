package client

import (
	"context"
	"net/http"
)

func (c *Client) Archive(ctx context.Context, id string) (*http.Response, error) {
	req, err := http.NewRequestWithContext(ctx, http.MethodPost, c.base+"/v1/bundles/"+id+"/archive", nil)
	if err != nil {
		return nil, err
	}
	return c.http.Do(req)
}

func Fetch(id string) (*http.Response, error) {
	return http.Get("https://api.example.com/v1/bundles/" + id)
}
