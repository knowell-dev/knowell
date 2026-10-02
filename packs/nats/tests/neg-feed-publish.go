package bus

type Feed struct{ items []string }

func (f *Feed) Publish(item string) {
	f.items = append(f.items, item)
}

func Fill(f *Feed) {
	f.Publish("not-a-subject")
}
