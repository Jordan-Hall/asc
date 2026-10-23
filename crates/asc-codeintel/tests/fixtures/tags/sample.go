package main

import "fmt"

// Server serves.
type Server struct {
	Addr string
	port int
}

type Handler interface {
	Serve(path string) error
}

type ID = int

const Max = 10

var global = 1

// Start begins serving.
func (s *Server) Start() error {
	var local int
	fmt.Println(s.Addr)
	return run(local)
}

func run(n int) error {
	return nil
}
