package main
import "github.com/gin-gonic/gin"
func main() {
    r := gin.Default()
    r.GET("/api/users", listUsers)
    r.POST("/api/users", createUser)
    r.GET("/api/users/:id", getUser)
    r.PUT("/api/users/:id", updateUser)
    r.DELETE("/api/users/:id", deleteUser)
}
func listUsers(c *gin.Context) {}
func createUser(c *gin.Context) {}
func getUser(c *gin.Context) {}
func updateUser(c *gin.Context) {}
func deleteUser(c *gin.Context) {}
