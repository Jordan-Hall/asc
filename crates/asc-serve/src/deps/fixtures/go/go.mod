module example.com/app

go 1.21

require github.com/gin-gonic/gin v1.9.0

require (
	golang.org/x/net v0.7.0 // indirect
	golang.org/x/text v0.7.0 // indirect
	github.com/stretchr/testify v1.8.1
	example.com/internal v0.0.0-20230101000000-abcdef123456
	example.com/replaced v1.0.0
	example.com/local v1.0.0
	"quoted.example/mod" v2.0.0+incompatible
)

replace example.com/replaced => example.com/replaced-fork v1.0.1

replace example.com/local => ../local

exclude golang.org/x/text v0.3.0

retract v0.9.0 // broken
