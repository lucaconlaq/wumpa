resource "google_project_service" "main" {
  for_each = toset([
    "compute.googleapis.com",
    "iam.googleapis.com",
    "iap.googleapis.com",
    "storage.googleapis.com",
  ])

  project            = google_project.main.project_id
  service            = each.value
  disable_on_destroy = false
}
