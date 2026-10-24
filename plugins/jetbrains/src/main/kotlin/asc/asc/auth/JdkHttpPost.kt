package asc.asc.auth

import java.net.URI
import java.net.http.HttpClient
import java.net.http.HttpRequest
import java.net.http.HttpResponse
import java.time.Duration

/** The token-endpoint POST. https only: any other scheme is refused before a socket opens. */
class JdkHttpPost : HttpPost {
    private val client = HttpClient.newBuilder()
        .connectTimeout(Duration.ofSeconds(10))
        .followRedirects(HttpClient.Redirect.NEVER)
        .build()

    override fun post(url: String, form: String): Pair<Int, String> {
        val uri = URI.create(url)
        require(uri.scheme == "https") { "https only" }
        val req = HttpRequest.newBuilder(uri).timeout(Duration.ofSeconds(20))
            .header("content-type", "application/x-www-form-urlencoded")
            .header("accept", "application/json")
            .POST(HttpRequest.BodyPublishers.ofString(form)).build()
        val res = client.send(req, HttpResponse.BodyHandlers.ofString())
        return res.statusCode() to res.body()
    }
}
