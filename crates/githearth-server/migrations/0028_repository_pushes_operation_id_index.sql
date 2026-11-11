-- Back the push-credential retention sweep's EXISTS lookup on
-- repository_pushes.operation_id (issue #110).
CREATE INDEX repository_pushes_operation_id ON repository_pushes (operation_id);
