-- 0001 accounts: down. Undo exactly what the up file does.
drop table password_resets;
drop table refresh_tokens;
drop table devices;
drop table invite_codes;
drop table users;
drop extension citext;
