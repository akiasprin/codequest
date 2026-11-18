package worker

import remote "example.test/structures/api"

type Service struct { *remote.Base; Name string }
type Other struct { Value int }
func (o Other) Handle(r remote.Request) int { return o.Value + r.Value }
type Box[T any] struct { Value T }
func (b Box[T]) Get() T { return b.Value }
type Alias = remote.Request

func Dispatch() int { return 1 }
func Run(s *Service, o Other) int {
    box := Box[int]{Value: 3}
    return Dispatch() + remote.Dispatch() + s.Handle(remote.Request{}) + o.Handle(remote.Request{}) + box.Get()
}
func ThroughInterface(h remote.Handler) int { return h.Handle(remote.Request{}) }
func MethodValue(s *Service) { callback := s.Handle; _ = callback }
func ReadLeft(v remote.Left) int { return v.Value }
func ReadRight(v remote.Right) int { return v.Value }
func Deferred() { callback := func() int { return Dispatch() }; _ = callback }
const FakeCode = "func Phantom() { Dispatch() }"

func Generic[T any](value T) T { return value }
func WrappedCalls() int {
    a := (Dispatch)()
    b := Generic[int](a)
    c := remote.Request{} // a composite literal is also not a function call
    _ = c
    return (remote.Dispatch)() + Generic[int](b)
}
func TypeConversion(value int) int { return int(value) }
