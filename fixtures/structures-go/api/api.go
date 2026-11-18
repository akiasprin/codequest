package api

type Request struct { Value int }
type Handler interface { Handle(Request) int }
type Base struct { Value int }
func (b *Base) Handle(r Request) int { return b.Value + r.Value }
func Dispatch() int { return 41 }
type Left struct { Value int }
type Right struct { Value int }
