# Bootstrap this project first, then link billing manually (see ../README.md).
resource "google_project" "main" {
  name                = var.project_name
  project_id          = var.project_id
  auto_create_network = false

  lifecycle {
    ignore_changes  = [billing_account]
    prevent_destroy = true
  }
}
