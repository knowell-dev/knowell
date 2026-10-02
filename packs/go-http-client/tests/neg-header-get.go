package client

import "net/http"

func Header(r *http.Request) string {
	return r.Header.Get("/v1/not-a-request")
}
