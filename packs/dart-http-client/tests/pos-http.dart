import 'package:http/http.dart' as http;

const baseUrl = 'https://api.example.com';

Future<http.Response> fetchTerminal(String id) {
  return http.get(Uri.parse('$baseUrl/v1/terminals/$id'));
}

Future<http.Response> createTerminal() {
  return http.post(Uri.parse('https://api.example.com/v1/terminals'), body: '{}');
}
