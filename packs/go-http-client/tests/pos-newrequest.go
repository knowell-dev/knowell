package client

import (
	"bytes"
	"net/http"
)

const statusURL = "https://status.example.com/v1/status"

func Status() (*http.Request, error) {
	return http.NewRequest("GET", statusURL, nil)
}

func Submit(body []byte) (*http.Response, error) {
	return http.Post("https://api.example.com/v1/submissions", "application/json", bytes.NewReader(body))
}
