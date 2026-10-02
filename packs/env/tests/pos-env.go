package config

import "os"

func Load() (string, bool) {
	dsn := os.Getenv("YARD_DSN")
	_, debug := os.LookupEnv("YARD_DEBUG")
	return dsn, debug
}

func getenv(name, fallback string) string {
	if v, ok := os.LookupEnv(name); ok {
		return v
	}
	return fallback
}

var addr = getenv("YARD_ADDR", ":8080")
