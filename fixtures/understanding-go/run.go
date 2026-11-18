// Package understanding models an output lifecycle.
package understanding

type Sink interface { Write(int) }
type Writer struct{}
func (Writer) Write(v int) { Emit(v) }
// Run coordinates output through Emit.
func Run(s Sink) {
 Emit(1)
 Emit(2)
 callback := Emit
 callback(3)
 f := func() { Emit(4) }
 f()
 s.Write(5)
}
