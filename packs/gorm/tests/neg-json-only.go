package model

// A plain struct with JSON tags is not a GORM model.
type Dimensions struct {
	Width  int `json:"width"`
	Height int `json:"height"`
}
