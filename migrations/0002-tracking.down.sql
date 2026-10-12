-- 0002 tracking: down. Undo exactly what the up file does; dropping points takes its partitions.
drop table markers;
drop table corrections;
drop table segments;
drop table points;
drop table places;
drop table upload_batches;
drop table import_batches;
